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
use crate::logger::Logger;
use crate::module_source::{CmNamespace, ModuleSource, ModuleSourceInterner, WasmAssetKind};
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
    /// A bare name matched a declared `[dependencies]` entry, but the
    /// dependency could not be resolved to an entry module (e.g. its package
    /// declares no `[package].lib`). `reason` explains why.
    DependencyUnresolved { name: String, reason: String },
    /// Wasm-asset import (`with { type: "wat"|"wasm" }`) failed validation.
    WasmImport {
        module_source: ModuleSource,
        message: String,
    },
    /// `#![stdlib("…")]` names no bundled stdlib module, or names nothing.
    StdlibIdentity {
        path: Option<String>,
        file: String,
        line: usize,
        column: usize,
    },
}

impl LoadError {
    /// Build a `LexError` from a recovered [`crate::lexer::LexError`].
    pub fn from_lex_error(e: &crate::lexer::LexError, module_source: ModuleSource) -> Self {
        LoadError::LexError {
            module_source,
            message: e.to_string(),
            line: e.span.line,
            column: e.span.column,
        }
    }

    /// Build a `ParseError` from a recovered [`crate::parser::ParseError`].
    pub fn from_parse_error(e: &crate::parser::ParseError, module_source: ModuleSource) -> Self {
        LoadError::ParseError {
            module_source,
            message: e.message.clone(),
            line: e.span.line,
            column: e.span.column,
        }
    }
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
                    "unknown module namespace '{namespace}'; expected 'core', 'wasi' or 'web'"
                )
            }
            LoadError::InvalidModulePath { path } => {
                write!(
                    f,
                    "invalid module path '{path}'; use './' for local modules or 'namespace:' for library modules"
                )
            }
            LoadError::DependencyUnresolved { name, reason } => {
                write!(f, "cannot resolve dependency '{name}': {reason}")
            }
            LoadError::WasmImport {
                module_source,
                message,
            } => {
                write!(f, "wasm import error in {module_source}: {message}")
            }
            LoadError::StdlibIdentity {
                path,
                file,
                line,
                column,
            } => {
                write!(
                    f,
                    "{file}: line {line}, column {column}: {}",
                    stdlib_identity_message(path.as_deref())
                )
            }
        }
    }
}

/// The one wording the display and the diagnostic share.
fn stdlib_identity_message(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "#![stdlib({path:?})] does not name a bundled stdlib module; the attribute \
             declares the identity of a module bundled in the compiler and is not for \
             use outside it"
        ),
        None => "#![stdlib] takes the name of a bundled stdlib module, as \
                 `#![stdlib(\"core:cli\")]`"
            .to_string(),
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
                    file: module_source.source_path(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                    space: crate::ast::AstIdSpace::FRESH,
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
                    file: module_source.source_path(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                    space: crate::ast::AstIdSpace::FRESH,
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
            LoadError::StdlibIdentity {
                ref path,
                ref file,
                line,
                column,
            } => Self {
                severity: Severity::Error,
                code: Code::ModuleNotFound,
                message: stdlib_identity_message(path.as_deref()),
                span: Some(DiagnosticSpan {
                    file: file.clone(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                    space: crate::ast::AstIdSpace::FRESH,
                }),
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

/// A core wasm value type, in the subset that can cross into Wado.
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
    /// Result types. 0 or 1 is accepted; multi-return is rejected.
    pub results: Vec<WasmCoreValType>,
}

/// Loaded wasm asset: post-`wat::parse_bytes` core wasm bytes plus the
/// extracted export signatures used by TIR synthesis.
#[derive(Debug, Clone)]
pub struct WasmAsset {
    /// Binary bytes (`.wat` is parsed at load time). For a component asset, the
    /// whole component binary, composed into the output at codegen.
    pub bytes: Vec<u8>,
    /// Function exports in wasm order. Empty for a component asset.
    pub function_exports: Vec<WasmExportSig>,
    /// Exported interface FQs; non-empty marks this as a CM component to compose in.
    pub component_interface_fqs: Vec<String>,
    /// Bare names of world-level function exports (Phase 9), wired by name at
    /// composition.
    pub component_world_func_names: Vec<String>,
}

impl WasmAsset {
    /// The minimum, in pages, of the memory the asset asks for — defined or
    /// imported; 1 when it has neither. Read by `wir_build` to size the memory
    /// the component shares, so an asset written against `env.memory` counts
    /// just as much as one defining its own.
    pub fn min_memory_pages(&self) -> u64 {
        for payload in wasmparser::Parser::new(0).parse_all(&self.bytes) {
            match payload {
                Ok(wasmparser::Payload::ImportSection(reader)) => {
                    for import in reader.into_imports().flatten() {
                        if let wasmparser::TypeRef::Memory(mem) = import.ty {
                            return mem.initial;
                        }
                    }
                }
                Ok(wasmparser::Payload::MemorySection(mems)) => {
                    if let Some(mem) = mems.into_iter().flatten().next() {
                        return mem.initial;
                    }
                }
                _ => {}
            }
        }
        1
    }
}

/// Resolve a `use` declaration's import source to its `ModuleSource`, mapping
/// the `with { type: "wat" | "wasm" }` form to `ModuleSource::Wasm`. Mirrors
/// `analyze::resolve_use_decl_module_source`.
pub fn resolve_use_decl_source(
    interner: &mut ModuleSourceInterner,
    from: &ModuleSource,
    use_decl: &crate::ast::UseDecl,
    entry: Option<&ModuleSource>,
    invocations: &crate::kiln::InvocationIndex,
) -> ModuleSource {
    if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref())
        && let Ok(path) =
            resolve_wasm_asset_path(from, &use_decl.source, &crate::name::entry_dir_of(entry))
    {
        return interner.wasm(&path, kind);
    }
    crate::name::resolve_import_with_invocations(
        interner,
        from,
        &use_decl.source,
        entry,
        invocations,
    )
}

/// Whether `bytes` is a CM component (vs a core module), per the preamble encoding.
fn is_wasm_component(bytes: &[u8]) -> bool {
    use wasmparser::{Encoding, Parser, Payload};
    Parser::new(0)
        .parse_all(bytes)
        .filter_map(Result::ok)
        .find_map(|payload| match payload {
            Payload::Version { encoding, .. } => Some(encoding == Encoding::Component),
            _ => None,
        })
        .unwrap_or(false)
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
    /// The CM interface each named-type reference in a decoded component's
    /// binding module resolves to, keyed by the reference's own site. The WIT
    /// importer is the only pass that knows a reference's precise owning
    /// interface, so it answers here rather than leaving a spelling behind.
    pub cm_source_interfaces: crate::component_model::SourceInterfaceBatch,
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

/// Build a `kiln:` URI from a filesystem path, normalizing and percent-encoding
/// it so the result is valid even for URI-unsafe paths (e.g. spaces);
/// `strip_kiln_scheme` reverses it losslessly. The single producer — the CLI
/// and LSP both call it, so their redirect URIs match.
///
/// `kiln:` (not `file://`) avoids a spurious `//`, which the qualified-name
/// format `{module_source}//{name}` treats as a boundary.
#[must_use]
pub fn path_to_kiln_uri(path: &str) -> String {
    use fluent_uri::pct_enc::{
        EString,
        encoder::{Data, Path},
    };
    // Encode per segment so `/` stays a separator.
    let encoded = crate::path::normalize(path)
        .split('/')
        .map(|segment| {
            let mut e = EString::<Path>::new();
            e.encode_str::<Data>(segment);
            e.into_string()
        })
        .collect::<Vec<_>>()
        .join("/");
    if encoded.starts_with('/') {
        format!("kiln:{encoded}")
    } else {
        // Keep the URI valid for a non-absolute input; the loader then fails to
        // find the file — a useful diagnostic shape.
        format!("kiln:/{encoded}")
    }
}

/// Strip a `kiln:` scheme, returning the percent-decoded path (inverse of
/// [`path_to_kiln_uri`]). Used by [`ModuleLoader::get_source`] for
/// `ModuleSource::Redirected`. Returns `None` for non-`kiln:` URIs, which
/// callers pass through unchanged (in-memory hosts key on the URI directly).
fn strip_kiln_scheme(uri: &str) -> Option<String> {
    let parsed = fluent_uri::UriRef::parse(uri).ok()?;
    if parsed.scheme()?.as_str() != "kiln" {
        return None;
    }
    Some(parsed.path().decode().to_string_lossy().into_owned())
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

/// Resolve a wasm asset import against the importing module's directory:
/// `./libm.wat` from `core:prelude/x.wado` gives `core:prelude/libm.wat`, from
/// `./sub/entry.wado` gives `./sub/libm.wat`. Only relative paths are accepted —
/// an absolute namespace-qualified target is unsupported from user code, and the
/// stdlib does not need one, its prelude importing siblings via `./`.
pub fn resolve_wasm_asset_path(
    from: &ModuleSource,
    import_source: &str,
    entry_dir: &str,
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
        ModuleSource::Binding {
            namespace,
            interface,
        } => Ok(format!(
            "{namespace}:{}",
            join_namespace_relative_path(interface, import_source)
        )),
        ModuleSource::Local { path } => Ok(crate::name::resolve_local_identity(
            entry_dir,
            path,
            import_source,
        )),
        ModuleSource::Dependency { path, .. } => Ok(resolve_module_path(path, import_source)),
        ModuleSource::Remote { url, .. } => Ok(resolve_module_path(url, import_source)),
        ModuleSource::EntryPoint { .. } => Ok(crate::name::canonical_local_path(
            entry_dir,
            &normalize_module_path(import_source),
        )),
        ModuleSource::Redirected { uri } => Ok(resolve_module_path(uri, import_source)),
        ModuleSource::Wasm { .. } => Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        }),
    }
}

/// Join `relative` onto the directory of `base` — a slash-separated
/// sub-namespace path such as `prelude/primitive.wado` — collapsing `..`
/// segments: `("prelude/primitive.wado", "./other.wat")` gives
/// `"prelude/other.wat"`. The result carries neither a namespace prefix nor a
/// leading `./`.
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

/// Synthesize Wado source declaring one extern `pub fn` per export of a wasm
/// asset, each carrying `#[canonical("<namespace>", "<export>")]` so the existing
/// import lowering picks the call up. Emitted as text and fed back through the
/// regular parse/bind pipeline. The identifiers are the wat export names
/// verbatim, so a re-exporter's `pub use { libm_sin, … }` lines up.
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

/// Log2 of the default wasm page size, 64 KiB.
const DEFAULT_PAGE_SIZE_LOG2: u32 = 16;

/// An embedded asset is wired to the component's memory, so its own memory
/// must have that memory's shape: 32-bit, unshared, default page size. Its
/// maximum needs no check — the rewrite to an import drops it, since the
/// component's memory is the one that sets the ceiling.
fn check_shared_memory_shape(
    source: &ModuleSource,
    mem: wasmparser::MemoryType,
) -> Result<(), LoadError> {
    if mem.memory64 {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "a 64-bit memory is not supported: the asset shares the component's \
                      32-bit memory"
                .to_string(),
        });
    }
    if mem.shared {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "a shared memory is not supported: the asset shares the component's \
                      unshared memory"
                .to_string(),
        });
    }
    // `None` and an explicit 64 KiB are the same shape; only a custom one is
    // a memory the component cannot hand over.
    if mem.page_size_log2.unwrap_or(DEFAULT_PAGE_SIZE_LOG2) != DEFAULT_PAGE_SIZE_LOG2 {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "a custom page size is not supported: the asset shares the component's \
                      memory, which uses the default 64 KiB page"
                .to_string(),
        });
    }
    Ok(())
}

/// Walk a core wasm module: validate what an embedded asset must be (no `start`,
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
                    if let wasmparser::TypeRef::Memory(mem) = import.ty {
                        check_shared_memory_shape(source, mem)?;
                        // Counted with the defined ones: only one memory can be
                        // wired to the component's.
                        memory_count += 1;
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
                    let mem = mem.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read memories: {e}"),
                    })?;
                    check_shared_memory_shape(source, mem)?;
                    memory_count += 1;
                }
            }
            _ => {}
        }
    }

    if had_start {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "a start section is not supported: the component instantiates the asset \
                      and never runs one"
                .to_string(),
        });
    }
    if memory_count > 1 {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: format!(
                "at most one memory is supported, found {memory_count}: the asset shares the \
                 component's memory"
            ),
        });
    }
    if !disallowed_imports.is_empty() {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: format!(
                "only the `env.memory` import is supported; found: {}",
                disallowed_imports.join(", ")
            ),
        });
    }
    // The asset is embedded in the component the compiler emits and codegen
    // walks its index spaces, so a module that only assembles is rejected here
    // rather than left to fail as an internal error later.
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(bytes)
        .map_err(|e| LoadError::WasmImport {
            module_source: source.clone(),
            message: format!("failed to validate wasm: {e}"),
        })?;

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
                         only i32/i64/f32/f64/v128 are supported"
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
                         only i32/i64/f32/f64/v128 are supported"
                    ),
                }
            })?);
        }
        if results.len() > 1 {
            return Err(LoadError::WasmImport {
                module_source: source.clone(),
                message: format!(
                    "export {name:?} has {} results; only 0 or 1 is supported",
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

/// Format a compiler error for a stdlib module: the kind, module and position,
/// then the offending source line with a caret under the column.
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
    let lex_result = crate::lexer::lex(source);
    if let Some(e) = lex_result.errors.first() {
        // Bundled stdlib must always lex cleanly; a recovered lex error here
        // is a compiler bug, so fail loudly rather than degrade.
        panic!(
            "{}",
            format_stdlib_error(
                "lexer",
                label,
                source,
                e.span.line,
                e.span.column,
                Some(e.span.end_column),
                &e.to_string(),
            )
        );
    }
    let mut parser = Parser::from_lex_no_trivia(lex_result);
    let ast = parser.parse();
    if let Some(e) = parser.take_errors().first() {
        // Bundled stdlib must always parse cleanly; a syntax error here is a
        // compiler bug, so fail loudly rather than degrade.
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
        );
    }
    {
        let bind_host = crate::compiler_host::InMemoryCompilerHost::new();
        let bind_logger = Logger::new(&bind_host, LogLevel::Off);
        // Only used for (unread) file attribution: a bind failure here is a
        // bundled-stdlib bug and the panic below formats against `label`.
        let module_source = crate::module_source::ModuleSourceInterner::new().entry_point(label);
        bind::bind_module(&ast, &module_source, &bind_logger).unwrap_or_else(|_| {
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

/// The bundled-stdlib identity `module` declares, `Ok(None)` when it declares
/// none. Naming nothing is an error wherever it is written — a file does not
/// become well-formed by being imported rather than compiled.
fn stdlib_identity_of<'a>(module: &'a Module, file: &str) -> Result<Option<&'a str>, LoadError> {
    let Some(attribute) = module.stdlib_identity_attribute() else {
        return Ok(None);
    };
    let path = module.stdlib_identity();
    match path.filter(|p| stdlib::get_stdlib_module(p).is_some()) {
        Some(path) => Ok(Some(path)),
        None => Err(LoadError::StdlibIdentity {
            path: path.map(str::to_string),
            file: file.to_string(),
            line: attribute.span.line,
            column: attribute.span.column,
        }),
    }
}

/// Resolve `module`'s `#![stdlib("…")]` declaration to a canonical
/// `ModuleSource`: it pins the entry's, which the rest of the compile resolves
/// against.
fn parse_stdlib_identity_attribute(
    interner: &mut ModuleSourceInterner,
    module: &Module,
    file: &str,
) -> Result<Option<ModuleSource>, LoadError> {
    let Some(path) = stdlib_identity_of(module, file)? else {
        return Ok(None);
    };
    if let Some(name) = path.strip_prefix("core:") {
        Ok(Some(interner.core(name)))
    } else if let Some((namespace, interface)) = CmNamespace::split_specifier(path) {
        Ok(Some(interner.binding(namespace, interface)))
    } else {
        unreachable!("a registered stdlib path carries `core:` or a reserved namespace")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::Parser;

    fn parse_test_module(source: &str) -> Module {
        let r = lex(source);
        assert!(r.errors.is_empty(), "test source must lex: {:?}", r.errors);
        let mut parser = Parser::from_lex_no_trivia(r);
        parser.parse_strict().expect("test source must parse")
    }

    #[test]
    fn kiln_uri_round_trips_uri_unsafe_paths() {
        // Valid URI out, lossless path back (regression for #1417).
        for path in [
            "/home/user/My Project/gen.wado",
            "/tmp/.wado-cache/gen.wado",
            "/a/b+c/d#e.wado",
            "/plain/path.wado",
        ] {
            let uri = path_to_kiln_uri(path);
            assert!(uri.starts_with("kiln:"), "scheme must be kiln: in `{uri}`");
            assert!(
                fluent_uri::UriRef::parse(uri.as_str()).is_ok(),
                "must be a valid URI: `{uri}`"
            );
            assert_eq!(strip_kiln_scheme(&uri).as_deref(), Some(path));
        }
    }

    #[test]
    fn strip_kiln_scheme_ignores_other_schemes() {
        assert_eq!(strip_kiln_scheme("core:cli"), None);
        assert_eq!(strip_kiln_scheme("./relative.wado"), None);
    }

    #[test]
    fn stdlib_identity_attribute_resolves_to_core_module_source() {
        let module = parse_test_module("#![no_prelude]\n#![stdlib(\"core:prelude/types.wado\")]\n");
        let mut interner = ModuleSourceInterner::new();
        let want = interner.core("prelude/types.wado");
        assert_eq!(
            parse_stdlib_identity_attribute(&mut interner, &module, "types.wado").ok(),
            Some(Some(want))
        );
    }

    #[test]
    fn unregistered_stdlib_identity_attribute_is_an_error() {
        let module = parse_test_module("#![no_prelude]\n#![stdlib(\"core:bogus.wado\")]\n");
        let mut interner = ModuleSourceInterner::new();
        let err = parse_stdlib_identity_attribute(&mut interner, &module, "bogus.wado")
            .expect_err("an unregistered path must be rejected");
        assert!(
            err.to_string()
                .contains("does not name a bundled stdlib module"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn stdlib_identity_attribute_without_a_name_is_an_error() {
        let module = parse_test_module("#![no_prelude]\n#![stdlib]\n");
        let err = stdlib_identity_of(&module, "nameless.wado")
            .expect_err("an omitted name must be rejected");
        assert!(
            err.to_string().contains("#![stdlib] takes the name of"),
            "unexpected message: {err}"
        );
    }
}

/// Per-module lazy cache for stdlib AST: each module is parsed and bound at most
/// once per process. The slot table is built eagerly on first access — just map
/// inserts of empty [`OnceLock`]s. Keyed by canonical display string rather than
/// `ModuleSource`, since every `ModuleLoader` interns through its own private
/// interner and this cache is process-global.
struct StdlibSlot {
    source: &'static str,
    module: std::sync::OnceLock<Module>,
}

type StdlibSlotMap = crate::hashmap::IndexMap<&'static str, StdlibSlot>;

fn stdlib_slots() -> &'static StdlibSlotMap {
    use std::sync::OnceLock;

    static SLOTS: OnceLock<StdlibSlotMap> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let core = stdlib::all_core_modules();
        let bindings = stdlib::all_binding_modules();
        let mut slots: StdlibSlotMap = crate::hashmap::IndexMap::with_capacity_and_hasher(
            core.len() + bindings.len(),
            FxBuildHasher,
        );
        for &(path, source) in core.iter().chain(bindings) {
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
    Some(
        slot.module
            .get_or_init(|| parse_bind_stdlib(key, slot.source)),
    )
}

/// [`stdlib_slots`] for a *bundled* wasm asset's synthesized bindings: its bytes
/// are fixed at build time, so it is parsed once per process too — an `AstId`
/// must mean the same node in every compile (WEP 2026-08-12 §1).
fn wasm_binding_slots()
-> &'static crate::hashmap::IndexMap<&'static str, std::sync::OnceLock<Module>> {
    use std::sync::OnceLock;

    static SLOTS: OnceLock<crate::hashmap::IndexMap<&'static str, OnceLock<Module>>> =
        OnceLock::new();
    SLOTS.get_or_init(|| {
        stdlib::ALL_CORE_WASM_ASSETS
            .iter()
            .map(|&(path, _)| (path, OnceLock::new()))
            .collect()
    })
}

/// The cached bindings AST for a bundled wasm asset; `None` for a host-loaded
/// one, whose bytes can change between compiles.
fn cached_wasm_binding_module(import_path: &str, source: &str) -> Option<&'static Module> {
    let (key, slot) = wasm_binding_slots().get_key_value(import_path)?;
    Some(slot.get_or_init(|| parse_bind_stdlib(key, source)))
}

/// Compute the cache key string used by [`cached_stdlib_module`] for a
/// `ModuleSource`. Returns `None` for variants that the stdlib cache
/// never holds (Local / Remote / `EntryPoint` / Redirected / Wasm).
fn stdlib_cache_key(ms: &ModuleSource) -> Option<String> {
    match ms {
        ModuleSource::Core { name } => Some(format!("core:{name}")),
        ModuleSource::Binding {
            namespace,
            interface,
        } => Some(format!("{namespace}:{interface}")),
        _ => None,
    }
}

/// Loads every module up front, before analysis and codegen, doing its I/O
/// through a `CompilerHost`.
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
    cm_source_interfaces: crate::component_model::SourceInterfaceBatch,
    /// Set of wasm asset namespace keys whose bytes are already in
    /// `wasm_assets` (used for dedup across multiple imports of the same
    /// asset).
    loaded_wasm_namespaces: IndexSet<String>,
    /// Wasm asset imports queued by `load_implicit_modules` (sync method);
    /// drained by `load_all` after that runs so the async fetch can happen.
    pending_implicit_wasm_imports: Vec<(ModuleSource, WasmAssetKind, crate::ast::UseDecl)>,
    /// Registry-component imports discovered during `collect_imports`: a
    /// coordinate `use { X } from "ns:pkg"` resolved to an already-materialized
    /// [`ModuleSource::Wasm`] via the dependency index. Drained like
    /// `pending_implicit_wasm_imports`, but from the resolved source directly.
    pending_component_imports: Vec<(ModuleSource, WasmAssetKind)>,
    /// Bundled stdlib packages a decoded CM component transitively imports (its
    /// host-leaf capabilities). Loaded once every component import is seen, so
    /// effect reconstruction can require the effects behind the component;
    /// otherwise an impure dependency's capability would go unrequested.
    pending_host_leaf_bindings: IndexSet<ModuleSource>,
    /// The entry module source (for dedup when sub-modules import back to entry)
    entry_module_source: Option<ModuleSource>,
    /// Canonical name of the entry module (e.g., "./`cross_module_type_identity.wado`")
    entry_canonical_name: Option<String>,
    /// Entry directory: the anchor for canonicalizing local module identities
    /// (see [`crate::name::canonical_local_path`]). Empty until the entry is
    /// loaded, or when the entry filename has no parent.
    entry_dir: String,
    /// Kiln invocation redirects: `(decl_file, from_path)` → generated entry
    /// module path. Consulted by `resolve_import` so a bare `use { X } from
    /// "<schema>"` picks up the generator's output.
    invocations: crate::kiln::InvocationIndex,
}

impl<'a, H: CompilerHost> ModuleLoader<'a, H> {
    /// Create a new module loader with the given host and log level
    pub fn new(host: &'a H, log_level: LogLevel) -> Self {
        let mut interner = ModuleSourceInterner::new();
        interner.set_dependencies(host.dependency_index());
        Self {
            host,
            log_level,
            logger: Logger::new(host, log_level),
            interner,
            loaded: IndexMap::default(),
            loading: IndexSet::default(),
            implicit_modules: IndexSet::default(),
            wasm_assets: IndexMap::default(),
            cm_source_interfaces: crate::component_model::SourceInterfaceBatch::default(),
            loaded_wasm_namespaces: IndexSet::default(),
            pending_implicit_wasm_imports: Vec::new(),
            pending_component_imports: Vec::new(),
            pending_host_leaf_bindings: IndexSet::default(),
            entry_module_source: None,
            entry_canonical_name: None,
            entry_dir: String::new(),
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
    /// It also loads implicit modules (core:prelude, core:rt, core:builtin).
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

        let entry_module_source = parse_stdlib_identity_attribute(
            &mut self.interner,
            &entry_ast,
            &tentative_entry_source.source_path(),
        )?
        .unwrap_or(tentative_entry_source);
        self.entry_module_source = Some(entry_module_source.clone());
        self.entry_canonical_name = Some(crate::name::canonicalize_entry_point(resolved_filename));
        self.entry_dir = crate::name::entry_dir_of(Some(&entry_module_source));

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

        // Load registry-component dependencies resolved from a coordinate `use`
        // (already-materialized `.wasm` paths from the dependency index).
        let components = std::mem::take(&mut self.pending_component_imports);
        for (source, kind) in components {
            self.handle_wasm_source(source, kind).await?;
        }

        // Now that every component import (file-path and registry-coordinate)
        // has been seen, load the WASI packages behind their host-leaf imports.
        self.load_pending_host_leaf_bindings();
        let queued = std::mem::take(&mut self.pending_implicit_wasm_imports);
        for (from_ms, kind, use_decl) in queued {
            self.handle_wasm_import(&from_ms, kind, &use_decl).await?;
        }

        self.check_stdlib_identities()?;

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
            cm_source_interfaces: self.cm_source_interfaces,
            invocations: self.invocations,
            interner: self.interner,
        })
    }

    /// Hold every loaded module to [`stdlib_identity_of`]'s rule. The entry's
    /// is resolved before its imports load; this reaches the rest.
    fn check_stdlib_identities(&self) -> Result<(), LoadError> {
        for (module_source, module) in &self.loaded {
            stdlib_identity_of(module, &module_source.source_path())?;
        }
        Ok(())
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
                // A coordinate resolving to a prebuilt component (registry
                // dependency) loads across the CM boundary, not as Wado source.
                if let ModuleSource::Wasm { kind, .. } = &resolved {
                    let kind = *kind;
                    self.pending_component_imports.push((resolved, kind));
                    continue;
                }
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
    /// declaration: validate what an embedded asset must be, resolve the asset
    /// path to a `ModuleSource::Wasm`, and load + record the bytes.
    ///
    /// Only the wildcard form (`use _ from "..."`) is handled here; named
    /// imports are rejected with a pointed diagnostic so users get a clear
    /// message instead of a downstream elaborator failure.
    async fn handle_wasm_import(
        &mut self,
        from_module_source: &ModuleSource,
        kind: WasmAssetKind,
        use_decl: &crate::ast::UseDecl,
    ) -> Result<(), LoadError> {
        let path = resolve_wasm_asset_path(from_module_source, &use_decl.source, &self.entry_dir)?;
        let source = self.interner.wasm(&path, kind);

        let _ = use_decl; // accepted for both wildcard and named forms; the
        // named form's items are resolved against the synthesized Wado
        // module produced below.

        self.handle_wasm_source(source, kind).await
    }

    /// Load a wasm asset from an already-resolved [`ModuleSource::Wasm`] — the
    /// shared body of [`Self::handle_wasm_import`], reused for a registry
    /// component dependency whose `.wasm` path is resolved from the dependency
    /// index rather than a `with { type: "wasm" }` clause.
    async fn handle_wasm_source(
        &mut self,
        source: ModuleSource,
        kind: WasmAssetKind,
    ) -> Result<(), LoadError> {
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
        let raw_bytes = {
            let _span = self.logger.span(&format!("load_wasm_asset {namespace}"));
            self.fetch_wasm_asset_bytes(&source, &path).await?
        };

        // A CM component takes the component path; a core module falls through.
        if kind == WasmAssetKind::Wasm && is_wasm_component(&raw_bytes) {
            return self.handle_component_import(&source, &namespace, raw_bytes);
        }

        let (core_wasm_bytes, function_exports) = {
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
        let ast = if let Some(cached) = cached_wasm_binding_module(&path, &synthesized_source) {
            cached.clone()
        } else {
            let ast = self
                .parse_source(&synthesized_source, &source)
                .inspect_err(|_e| {
                    self.logger.span_end(&span);
                })?;
            self.bind_module(&ast, &source).inspect_err(|_e| {
                self.logger.span_end(&span);
            })?;
            ast
        };
        self.logger.span_end(&span);
        self.loaded.insert(source.clone(), ast);

        self.loaded_wasm_namespaces.insert(namespace.clone());
        self.wasm_assets.insert(
            namespace,
            WasmAsset {
                bytes: core_wasm_bytes,
                function_exports,
                component_interface_fqs: Vec::new(),
                component_world_func_names: Vec::new(),
            },
        );
        Ok(())
    }

    /// Decode the component's WIT into a `#[cm(...)]` binding module and record
    /// its bytes for composition at codegen. The synthesized module flows
    /// through the normal frontend, so the import resolves like any other module.
    fn handle_component_import(
        &mut self,
        source: &ModuleSource,
        namespace: &str,
        bytes: Vec<u8>,
    ) -> Result<(), LoadError> {
        let span = format!("decode component {namespace}");
        self.logger.span_start(&span);
        let built = self.build_component_bindings(source, &bytes);
        self.logger.span_end(&span);
        let bindings = built?;

        // Queue the bundled packages behind this component's host-leaf imports
        // so effect reconstruction sees the effects it transitively needs (a
        // `wasi:clocks/monotonic-clock@…` import loads the `clocks` package).
        for fq in &bindings.host_leaf_imports {
            if let Some((namespace, rest)) = CmNamespace::split_specifier(fq) {
                let package = rest.split('/').next().unwrap_or(rest);
                let ms = self.interner.binding(namespace, package);
                self.pending_host_leaf_bindings.insert(ms);
            }
        }

        self.cm_source_interfaces.extend(bindings.source_interfaces);
        self.bind_module(&bindings.module, source)?;
        self.loaded.insert(source.clone(), bindings.module);
        self.loaded_wasm_namespaces.insert(namespace.to_string());
        self.wasm_assets.insert(
            namespace.to_string(),
            WasmAsset {
                bytes,
                function_exports: Vec::new(),
                component_interface_fqs: bindings.interface_fqs,
                component_world_func_names: bindings.world_func_names,
            },
        );
        Ok(())
    }

    fn build_component_bindings(
        &self,
        source: &ModuleSource,
        bytes: &[u8],
    ) -> Result<crate::wit_consume::ComponentBindings, LoadError> {
        let err = |message: String| LoadError::WasmImport {
            module_source: source.clone(),
            message,
        };
        let decoded = wit_component::decode(bytes)
            .map_err(|e| err(format!("failed to decode component WIT: {e}")))?;
        let (resolve, world) = match decoded {
            wit_component::DecodedWasm::Component(resolve, world) => (resolve, world),
            wit_component::DecodedWasm::WitPackage(..) => {
                return Err(err("expected a component, found a WIT package".to_string()));
            }
        };
        crate::wit_consume::build_bindings(&resolve, world).map_err(err)
    }

    /// Fetch the raw bytes of a wasm asset by canonical path. Stdlib paths
    /// (`core:libm.wat`, etc.) hit `stdlib::get_stdlib_wasm_asset`; user
    /// paths (`./foo.wat`) hit `host.load_source`.
    async fn fetch_wasm_asset_bytes(
        &self,
        source: &ModuleSource,
        path: &str,
    ) -> Result<Vec<u8>, LoadError> {
        if crate::module_source::is_bundled_specifier(path) {
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
        self.load_stdlib_sources(vec![
            ModuleSource::builtin(),
            ModuleSource::string(),
            ModuleSource::prelude(),
            ModuleSource::rt(),
            ModuleSource::allocator(),
        ]);
        Ok(())
    }

    /// Load the WASI stdlib packages behind imported components' host-leaf
    /// capabilities so their effects are in scope for reconstruction. Runs
    /// after every component import — file-path (`with { type: "wasm" }`) and
    /// registry-coordinate — has been processed, since a coordinate dependency
    /// is drained after `load_implicit_modules` yet still contributes host-leaf
    /// imports; loading here catches both paths in one place.
    fn load_pending_host_leaf_bindings(&mut self) {
        let sources: Vec<ModuleSource> = std::mem::take(&mut self.pending_host_leaf_bindings)
            .into_iter()
            .collect();
        self.load_stdlib_sources(sources);
    }

    /// Load each stdlib `module_source` (and its transitive stdlib deps) from
    /// the bundled cache, recording them as implicit modules. Already-loaded or
    /// non-stdlib sources are skipped; wasm-asset imports they surface are
    /// queued in `pending_implicit_wasm_imports` for the caller to drain.
    fn load_stdlib_sources(&mut self, sources: Vec<ModuleSource>) {
        for module_source in sources {
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
    }

    /// Emit a `Code::KilnMissingWith` diagnostic for a bare `use ... from
    /// "./schema.<ext>"` whose source is a non-`.wado` schema and that has
    /// no inline `with { generator: { ... } }` clause registered for this
    /// importing file. WEP 2026-04-12 §"Use-site syntax" makes such
    /// imports a hard error so the user gets a pointed message instead of
    /// a downstream parse failure on the schema content.
    fn emit_kiln_missing_with(
        &self,
        from_module_source: &ModuleSource,
        use_decl: &crate::ast::UseDecl,
    ) {
        use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
        let file = match from_module_source {
            ModuleSource::Local { path } | ModuleSource::Dependency { path, .. } => {
                path.to_string()
            }
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
                ModuleSource::Local { path } | ModuleSource::Dependency { path, .. } => {
                    path.as_str()
                }
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
        if let Some((namespace, interface)) = CmNamespace::split_specifier(import_source) {
            return Ok(self.interner.binding(namespace, interface));
        }

        // Handle remote modules (http:// or https://)
        if import_source.starts_with("https://") || import_source.starts_with("http://") {
            return Ok(self.interner.remote(import_source));
        }

        // Handle local modules (./ or ../)
        if import_source.starts_with("./") || import_source.starts_with("../") {
            // For relative imports, resolve against from_module_source
            if let ModuleSource::Local { path: from_file } = from_module_source {
                let resolved =
                    crate::name::resolve_local_identity(&self.entry_dir, from_file, import_source);
                // Fold a back-reference to the entry onto its EntryPoint identity.
                if let Some(ref entry_canonical) = self.entry_canonical_name
                    && resolved == *entry_canonical
                    && let Some(ref entry_ms) = self.entry_module_source
                {
                    return Ok(entry_ms.clone());
                }
                return Ok(self.interner.local(&resolved));
            }
            if let ModuleSource::Remote { pkg, url: from_url } = from_module_source {
                let resolved = resolve_module_path(from_url, import_source);
                let pkg = pkg.to_string();
                return Ok(self.interner.remote_module(&pkg, &resolved));
            }
            // A relative import inherits the importer's package root.
            if let ModuleSource::Dependency { pkg, path } = from_module_source {
                let resolved = resolve_module_path(path, import_source);
                let pkg = pkg.to_string();
                return Ok(self.interner.dependency_module(&pkg, &resolved));
            }
            // Entry imports canonicalize against the entry dir; stdlib / bare
            // relative imports are not anchored there, so they only normalize.
            if matches!(from_module_source, ModuleSource::EntryPoint { .. }) {
                let resolved = crate::name::canonical_local_path(
                    &self.entry_dir,
                    &normalize_module_path(import_source),
                );
                return Ok(self.interner.local(&resolved));
            }
            let canonical = normalize_module_path(import_source);
            return Ok(self.interner.local(&canonical));
        }

        // Bare dependency name: resolve against `[dependencies]`. Only the
        // consuming project resolves its own deps; a bare import from within
        // a dependency must not bind to the consumer's deps.
        if !matches!(from_module_source, ModuleSource::Dependency { .. }) {
            if let Some(dep) = self.interner.resolve_dependency(import_source) {
                return Ok(dep);
            }
            // A registry dependency is a prebuilt component: resolve to a
            // `Wasm` source pointing at its fetched `.wasm`, imported across the
            // CM boundary like a `with { type: "wasm" }` asset.
            if let Some(component) = self.interner.resolve_component_dependency(import_source) {
                return Ok(component);
            }
            // Declared but unresolvable (e.g. missing `[package].lib`): report
            // why, instead of a generic "invalid module path".
            if let Some(reason) = self.interner.unresolved_dependency(import_source) {
                return Err(LoadError::DependencyUnresolved {
                    name: import_source.to_string(),
                    reason: reason.to_string(),
                });
            }
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
            ModuleSource::Local { path } | ModuleSource::Dependency { path, .. } => {
                let bytes = self.host.load_source(path).await.map_err(LoadError::from)?;
                String::from_utf8(bytes).map_err(|_| LoadError::IoError {
                    path: path.to_string(),
                    message: "file is not valid UTF-8".to_string(),
                })
            }
            ModuleSource::Remote { url, .. } => {
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
            ModuleSource::Binding {
                namespace,
                interface,
            } => {
                let import_path = format!("{namespace}:{interface}");
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
        let lex_result = crate::lexer::lex(source);
        if let Some(e) = lex_result.errors.first() {
            return Err(LoadError::from_lex_error(e, module_source.clone()));
        }

        let mut parser = Parser::from_lex_no_trivia(lex_result);
        // Batch loading is fail-fast: report the first recovered syntax error
        // as a load error so compilation never proceeds on a partial AST.
        let ast = parser.parse();
        if let Some(e) = parser.take_errors().first() {
            return Err(LoadError::from_parse_error(e, module_source.clone()));
        }
        Ok(ast)
    }

    /// Bind a module (local name resolution and scope checking)
    fn bind_module(&self, module: &Module, module_source: &ModuleSource) -> Result<(), LoadError> {
        // Bind errors are emitted directly to the host via Logger.
        // We use a temporary logger per module so error counting is per-module.
        let logger = Logger::new(self.host, self.log_level);
        bind::bind_module(module, module_source, &logger).map_err(|_bail| {
            let error_count = logger.error_count();
            LoadError::BindError {
                module_source: module_source.clone(),
                message: format!("{error_count} bind error(s)"),
            }
        })
    }

    /// Scan all loaded modules for `#include_str`/`#include_bytes` and load referenced files.
    async fn load_included_files(&self) -> Result<IndexMap<[String; 2], Vec<u8>>, LoadError> {
        // Collect (module_source, raw_path) pairs. The map key stays the
        // module's `Display` (matching the elaborator's `included_files`
        // lookup), but the include path resolves against the module's real
        // filename via `source_path`: an entry point's `Display` is its
        // stable base name for symbol identity, which drops the directory a
        // relative include needs.
        let mut pairs: IndexSet<[String; 2]> = IndexSet::default();
        let mut resolve_path: IndexMap<String, String> = IndexMap::default();
        for (module_source, module) in &self.loaded {
            let ms_str = module_source.to_string();
            for raw_path in module.include_paths() {
                resolve_path
                    .entry(ms_str.clone())
                    .or_insert_with(|| module_source.source_path());
                pairs.insert([ms_str.clone(), raw_path.clone()]);
            }
        }
        // Format the entry module source once so the per-include elaborator
        // can compare against it without allocating per call.
        let entry_module_source = self
            .entry_module_source
            .as_ref()
            .map(ModuleSource::source_path);
        let mut included = IndexMap::default();
        for pair in pairs {
            let [ref ms_str, ref raw_path] = pair;
            // Resolve path relative to the module source's real directory.
            let module_path = resolve_path
                .get(ms_str)
                .map_or(ms_str.as_str(), String::as_str);
            let resolved =
                resolve_include_path_impl(entry_module_source.as_deref(), module_path, raw_path);
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

/// Resolve an include path relative to its containing module, returning what
/// `CompilerHost::load_source` should join onto `base_path`. Whether to keep the
/// module's directory as a prefix is [`dir_prefix_to_keep`]'s decision: a
/// cwd-relative entry already encodes what the host's join re-introduces, while
/// a `./`-rooted import needs the prefix to stay inside its own directory.
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

/// Whether an include path keeps its module's directory as a prefix, or collapses
/// to the bare filename. Two cases collapse, both because re-prepending `dir`
/// would duplicate what `base_path.join` adds: the cwd-relative entry module,
/// whose `dir` *is* `base_path`, and any module whose `dir` does not look
/// base_path-relative. Absolute paths and `./`-rooted imports keep the prefix.
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
    fn cross_package_dependency_include_resolves_in_the_dependency_dir() {
        // A `#include_str` in a dependency module must resolve next to that
        // module, not under the consumer's `base_path`. `source_path` now hands
        // the dependency its real base-relative path (`../dep/…`, not
        // `dep:../dep/…`), so the dir prefix is kept and `base_path.join`
        // lands in the dependency's directory.
        let entry = "./src/main.wado";
        let dep_module = "../dep/src/highlight/facade.wado";
        let resolved = resolve(Some(entry), dep_module, "./themes/gale-light.css");
        assert_eq!(resolved, "../dep/src/highlight/themes/gale-light.css");
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
