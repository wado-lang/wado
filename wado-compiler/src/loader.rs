//! Module loader for Wado
//!
//! Loads all modules (entry module + dependencies) upfront before analysis.
//! This enables converting ALL modules to TIR before codegen.

use std::collections::VecDeque;

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;
use rustc_hash::FxBuildHasher;

use crate::ast::{AstIdSpace, ImportAttributes, Item, Module, UseDecl};
use crate::compiler_host::{
    Code, CompilerHost, DependencyManifest, Diagnostic, DiagnosticSpan, InMemoryCompilerHost,
    Severity, SourceError,
};
use crate::component_model::SourceInterfaceBatch;
use crate::kiln::InvocationIndex;
use crate::lexer::{LexError, is_valid_ident, lex};
use crate::logger::Logger;
use crate::module_source::{
    CmNamespace, ExportRefusal, ModuleSource, ModuleSourceInterner, WasmAssetKind,
    is_bundled_specifier,
};
use crate::name::{
    canonicalize_entry_point, decl_file_of, entry_dir_of, is_package_namespace,
    resolve_import_with_invocations, resolve_local_identity, resolve_module_path,
    split_exported_file, wado_identifier,
};
use crate::parser::Parser;
use crate::path::{is_cwd_relative, normalize};
use crate::remote::{
    Integrity, PinnedBy, check_digest, is_remote, next_in_pin_order, pin_of, url_fault,
};
use crate::wit_consume::{ComponentBindings, ExportedInterface, build_bindings};
use crate::{bind, hashmap, parser, stdlib};

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
    /// A file an `#include_str` or `#include_bytes` names is not there.
    IncludeNotFound { path: String },
    /// An `#include_*` path that is not `./` or `../` relative.
    InvalidIncludePath { path: String },
    /// A remote include where no lock governs the module: no `use` names an
    /// include, so nothing else can pin it.
    UnpinnedInclude { url: String },
    /// A namespaced import (`ns:pkg`, `lib:nick`) that no dependency declares.
    /// Every namespace but the bundled `core` and `wasi` is one a dependency
    /// supplies. `manifest` is the `wado.toml` it would be declared in.
    UndeclaredDependency {
        name: String,
        manifest: Option<String>,
    },
    /// Invalid module path format (e.g., "foo.wado" without "./" prefix)
    InvalidModulePath { path: String },
    /// A URL naming no file Wado fetches, for `reason`.
    InvalidRemoteUrl { url: String, reason: &'static str },
    /// A `with { integrity }` value that pins nothing.
    InvalidIntegrity { message: String },
    /// A remote file whose content its pin refuses, or that nothing pins: a
    /// [`SourceError::IntegrityMismatch`] or [`SourceError::Unpinned`].
    RemoteIntegrity(SourceError),
    /// A bare name matched a declared `[dependencies]` entry, but the
    /// dependency could not be resolved to an entry module (e.g. its package
    /// declares no `[package].lib`). `reason` explains why.
    DependencyUnresolved { name: String, reason: String },
    /// `"<package>/<file>"` names a file the package's `[package].exports`
    /// does not list. Worded at package granularity, so it never says whether
    /// the file is there.
    UnexportedFile {
        package: String,
        file: String,
        refusal: ExportRefusal,
    },
    /// Wasm-asset import (`with { type: "wat"|"wasm" }`) failed validation.
    WasmImport {
        module_source: ModuleSource,
        message: String,
    },
    /// `error` raised loading what a `use` or an `#include_*` names, reported
    /// at the path it names.
    AtPath {
        at: DiagnosticSpan,
        error: Box<LoadError>,
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
    pub fn from_lex_error(e: &LexError, module_source: ModuleSource) -> Self {
        LoadError::LexError {
            module_source,
            message: e.to_string(),
            line: e.span.line,
            column: e.span.column,
        }
    }

    /// Build a `ParseError` from a recovered [`crate::parser::ParseError`].
    pub fn from_parse_error(e: &parser::ParseError, module_source: ModuleSource) -> Self {
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
            LoadError::IncludeNotFound { path } => write!(f, "file not found: '{path}'"),
            LoadError::InvalidIncludePath { path } => {
                write!(
                    f,
                    "invalid include path '{path}'; an include path starts with './' or '../'"
                )
            }
            LoadError::UnpinnedInclude { url } => write!(
                f,
                "cannot pin '{url}': only a `wado.lock` pins a remote module's include, \
                 and none governs this module"
            ),
            LoadError::UndeclaredDependency {
                name,
                manifest: Some(manifest),
            } => {
                write!(
                    f,
                    "dependency '{name}' is not declared in [dependencies] of {manifest}"
                )
            }
            LoadError::UndeclaredDependency {
                name,
                manifest: None,
            } => {
                write!(
                    f,
                    "dependency '{name}' is not declared: no wado.toml governs this module, \
                     and the `use` gives no inline source"
                )
            }
            LoadError::InvalidModulePath { path } => {
                write!(
                    f,
                    "invalid module path '{path}'; use './' for local modules or 'namespace:' for library modules"
                )
            }
            LoadError::InvalidRemoteUrl { url, reason } => {
                write!(f, "invalid module path '{url}': {reason}")
            }
            LoadError::RemoteIntegrity(error) => error.fmt(f),
            LoadError::InvalidIntegrity { message } => f.write_str(message),
            LoadError::DependencyUnresolved { name, reason } => {
                write!(f, "cannot resolve dependency '{name}': {reason}")
            }
            LoadError::UnexportedFile {
                package,
                file,
                refusal,
            } => {
                write!(f, "package `{package}` exports no file `{file}`")?;
                match refusal {
                    ExportRefusal::NotExported {
                        spelled_alike: Some(entry),
                    } => write!(f, "; it exports `{entry}`, which differs in case"),
                    ExportRefusal::NotExported {
                        spelled_alike: None,
                    } => Ok(()),
                    ExportRefusal::Component => {
                        write!(f, "; it is a prebuilt component, which carries no files")
                    }
                }
            }
            LoadError::WasmImport {
                module_source,
                message,
            } => {
                write!(f, "wasm import error in {module_source}: {message}")
            }
            LoadError::AtPath { error, .. } => error.fmt(f),
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

impl LoadError {
    pub(crate) fn at_path(self, at: DiagnosticSpan) -> Self {
        LoadError::AtPath {
            at,
            error: Box::new(self),
        }
    }

    /// A failure reading a file, naming it `name` rather than as the host
    /// resolved it, absolute or joined onto a `./` ([`read_name`]).
    pub(crate) fn reading(err: SourceError, name: &str) -> Self {
        let path = name.to_string();
        match err {
            SourceError::NotFound { .. } => LoadError::ModuleNotFound { path },
            SourceError::IoError { message, .. } | SourceError::NetworkError { message, .. } => {
                LoadError::IoError { path, message }
            }
            err @ (SourceError::IntegrityMismatch { .. } | SourceError::Unpinned { .. }) => {
                LoadError::RemoteIntegrity(err)
            }
        }
    }

    /// [`Self::reading`] for the file an `#include_*` names.
    fn reading_include(err: SourceError, name: &str) -> Self {
        match err {
            SourceError::NotFound { .. } => LoadError::IncludeNotFound {
                path: name.to_string(),
            },
            SourceError::Unpinned { url, .. } => LoadError::UnpinnedInclude { url },
            err => Self::reading(err, name),
        }
    }

    /// The code for a variant whose diagnostic message is its `Display` text.
    fn code(&self) -> Code {
        match self {
            LoadError::BindError { .. } => Code::DuplicateDefinition,
            LoadError::WasmImport { .. } => Code::InvalidSyntax,
            LoadError::AtPath { error, .. } => error.code(),
            LoadError::LexError { .. } | LoadError::ParseError { .. } => Code::InvalidSyntax,
            LoadError::IoError { .. } | LoadError::IncludeNotFound { .. } => Code::FileReadError,
            LoadError::StdlibIdentity { .. } => Code::StdlibAttr,
            LoadError::RemoteIntegrity(_) | LoadError::UnpinnedInclude { .. } => {
                Code::RemoteIntegrity
            }
            LoadError::InvalidIntegrity { .. } => Code::ImportAttrInvalid,
            LoadError::ModuleNotFound { .. }
            | LoadError::UndeclaredDependency { .. }
            | LoadError::InvalidModulePath { .. }
            | LoadError::InvalidIncludePath { .. }
            | LoadError::InvalidRemoteUrl { .. }
            | LoadError::DependencyUnresolved { .. }
            | LoadError::UnexportedFile { .. } => Code::ModuleNotFound,
        }
    }
}

impl From<LoadError> for Diagnostic {
    fn from(e: LoadError) -> Self {
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
                    space: AstIdSpace::FRESH,
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
                    space: AstIdSpace::FRESH,
                }),
            },
            LoadError::StdlibIdentity {
                ref path,
                ref file,
                line,
                column,
            } => Self {
                severity: Severity::Error,
                code: Code::StdlibAttr,
                message: stdlib_identity_message(path.as_deref()),
                span: Some(DiagnosticSpan {
                    file: file.clone(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                    space: AstIdSpace::FRESH,
                }),
            },
            LoadError::AtPath { at, error } => Self {
                span: Some(at),
                ..Self::from(*error)
            },
            ref other => Self {
                severity: Severity::Error,
                code: other.code(),
                message: other.to_string(),
                span: None,
            },
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
    /// The name the bindings declare it under ([`wado_identifier`]), as a
    /// Component Model function's: `add-one` and `addOne` read as `add_one`.
    pub binding: String,
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
    /// Exported interfaces; non-empty marks this as a CM component to compose in.
    pub component_interfaces: Vec<ExportedInterface>,
    /// The CM function path of every function a component asset imports.
    pub imported_functions: Vec<String>,
}

impl WasmAsset {
    /// The minimum, in pages, of the memory the asset asks for — defined or
    /// imported; 1 when it has neither. Read by
    /// [`crate::nir_package::NirPackage::reserve_asset_memory`] to reserve the
    /// asset's pages of the memory the component shares, so an asset written
    /// against `env.memory` counts just as much as one defining its own.
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

/// The module `use_decl` imports from `from`, a Wasm asset where its `with`
/// says so. `None` for a malformed asset path, which analysis reports.
pub fn resolve_use_decl_source(
    interner: &mut ModuleSourceInterner,
    from: &ModuleSource,
    use_decl: &UseDecl,
    entry: Option<&ModuleSource>,
    invocations: &InvocationIndex,
) -> Option<ModuleSource> {
    if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()) {
        return resolve_wasm_asset_path(from, &use_decl.source, &entry_dir_of(entry))
            .ok()
            .map(|path| interner.wasm(&path, kind));
    }
    Some(resolve_import_with_invocations(
        interner,
        from,
        &use_decl.source,
        entry,
        invocations,
    ))
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
    pub cm_source_interfaces: SourceInterfaceBatch,
    /// Kiln invocation redirects propagated from the loader so later phases
    /// (analyze, elaborator) can also rewrite `use ... from "<schema>"`
    /// clauses consistently.
    pub invocations: InvocationIndex,
    /// `ModuleSource` interner created during loading. Downstream phases
    /// (analyze / elaborator / synthesis / monomorphize) borrow this to
    /// canonicalize any `ModuleSource` they construct so that ptr-eq
    /// remains a valid identity check across phases.
    pub interner: ModuleSourceInterner,
    /// The modules whose function bodies were read as empty blocks
    /// ([`ModuleLoader::with_generated_bodies_skipped`]).
    pub skipped_bodies: IndexSet<ModuleSource>,
    /// The Wado modules each module's `use` declarations load, in order.
    pub imports: IndexMap<ModuleSource, Vec<ModuleSource>>,
    /// Each wasm asset's file -> the asset path of the first `use` importing
    /// it, where a diagnostic in its bindings is reported
    /// ([`Logger::register_asset_origins`]).
    pub asset_origins: IndexMap<String, PathSite>,
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
    let encoded = normalize(path)
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

/// Extract the wasm-asset kind from a use declaration's `with { ... }`
/// attributes. Returns `Some(kind)` for `with { type: "wat" | "wasm" }`,
/// `None` otherwise (including for unrelated `with { ... }` attributes
/// such as `with { version: "1.0" }`). A `type` beside `generator` is the
/// generator's to read, so that import is no asset.
pub fn wasm_asset_kind_from_attrs(attrs: Option<&ImportAttributes>) -> Option<WasmAssetKind> {
    let attrs = attrs?;
    if attrs.is_generated() {
        return None;
    }
    let type_hint = ImportAttributes::type_hint(attrs)?;
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
    if is_remote(import_source) {
        return Ok(import_source.to_string());
    }
    if !is_cwd_relative(import_source) {
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
        ModuleSource::Local { path, .. } => {
            Ok(resolve_local_identity(entry_dir, Some(path), import_source))
        }
        ModuleSource::Dependency { path, .. } => Ok(resolve_module_path(path, import_source)),
        ModuleSource::Remote { url, .. } => Ok(resolve_module_path(url, import_source)),
        ModuleSource::EntryPoint { .. } => {
            Ok(resolve_local_identity(entry_dir, None, import_source))
        }
        ModuleSource::Redirected { uri, .. } => Ok(resolve_module_path(uri, import_source)),
        ModuleSource::Wasm { .. } => Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        }),
        ModuleSource::Effect { .. } => unreachable!("an effect namespace imports nothing"),
    }
}

/// A path as a `use` wrote it, and where: an error loading what it names is
/// reported there.
#[derive(Debug, Clone)]
pub struct PathSite {
    pub written: String,
    pub at: DiagnosticSpan,
}

/// What an error reading a file names it. A relative path names the file it
/// reads, so the error says it as written rather than as the host joined it.
/// A coordinate or a URL names the file it resolved to.
fn read_name<'a>(written: &'a str, resolved: &'a str) -> &'a str {
    if is_cwd_relative(written) {
        written
    } else {
        resolved
    }
}

/// `bytes` as UTF-8 text, naming the file `name` if they are not.
fn utf8_text(bytes: Vec<u8>, name: &str) -> Result<String, LoadError> {
    String::from_utf8(bytes).map_err(|_| LoadError::IoError {
        path: name.to_string(),
        message: "file is not valid UTF-8".to_string(),
    })
}

/// The pin a `with { integrity: "…" }` writes, where its value is written.
fn written_pin(
    attrs: Option<&ImportAttributes>,
    from: &ModuleSource,
) -> Result<Option<(Integrity, DiagnosticSpan)>, LoadError> {
    let Some(entry) = attrs.and_then(ImportAttributes::integrity) else {
        return Ok(None);
    };
    let at = DiagnosticSpan::from_span(&entry.value_span, Some(&from.source_path()));
    match pin_of(&entry.value) {
        Ok(pin) => Ok(Some((pin, at))),
        Err(reason) => Err(LoadError::InvalidIntegrity {
            message: format!("import attribute `integrity` {reason}"),
        }
        .at_path(at)),
    }
}

fn use_site(from: &ModuleSource, use_decl: &UseDecl) -> PathSite {
    PathSite {
        written: use_decl.source.to_string(),
        at: DiagnosticSpan::from_span(&use_decl.source_span, Some(&from.source_path())),
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
/// import lowering picks the call up, and the facts of an opaque import. Emitted
/// as text and fed back through the regular parse/bind pipeline. The identifiers are the wat export names
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
        // A core Wasm export exchanges only scalars, and its body is out of sight.
        let _ = writeln!(out, "#[storage(none)]");
        let _ = writeln!(out, "#[side_effect(opaque)]");
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
                    sig.binding,
                    wasm_core_val_type_name(*ret),
                );
            }
            None => {
                let _ = writeln!(out, "pub fn {}({params});", sig.binding);
            }
        }
    }
    out
}

/// Log2 of the default wasm page size, 64 KiB.
pub(crate) const DEFAULT_PAGE_SIZE_LOG2: u32 = 16;

/// An embedded asset is wired to the component's memory, so its own memory
/// must have that memory's shape: 32-bit, unshared, default page size, and a
/// minimum that leaves the heap room past it. Its maximum needs no check — the
/// rewrite to an import drops it, since the component's memory is the one that
/// sets the ceiling.
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
    if mem.initial >= MAX_ASSET_MEMORY_PAGES {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "a memory of 2 GiB or more is not supported: the component's heap starts \
                      past the asset's memory, within the 32-bit address space"
                .to_string(),
        });
    }
    Ok(())
}

/// The smallest memory minimum, in pages, that rejects an asset. The heap
/// starts past the asset's pages, and that address must fit a positive `i32`.
const MAX_ASSET_MEMORY_PAGES: u64 = 1 << (31 - DEFAULT_PAGE_SIZE_LOG2);

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
    let mut bound: IndexMap<String, String> = IndexMap::default();
    for (name, func_index) in exports {
        let binding = wado_identifier(&name);
        if !is_valid_ident(&binding) {
            return Err(LoadError::WasmImport {
                module_source: source.clone(),
                message: format!(
                    "export `{name}` reads as `{binding}`, which is no Wado identifier"
                ),
            });
        }
        if let Some(first) = bound.insert(binding.clone(), name.clone()) {
            return Err(LoadError::WasmImport {
                module_source: source.clone(),
                message: format!("exports `{first}` and `{name}` both read as `{binding}`"),
            });
        }
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
                message: format!("export `{name}` references missing function index"),
            })?;
        let func_ty = func_types
            .get(type_idx as usize)
            .ok_or_else(|| LoadError::WasmImport {
                module_source: source.clone(),
                message: format!("export `{name}` references missing type index"),
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
            binding,
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
    let lex_result = lex(source);
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
        let bind_host = InMemoryCompilerHost::new();
        let bind_logger = Logger::new(&bind_host, LogLevel::Off);
        // Only used for (unread) file attribution: a bind failure here is a
        // bundled-stdlib bug and the panic below formats against `label`.
        let module_source = ModuleSourceInterner::new().entry_point(label);
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

    /// The attributes of the module's one `use`.
    fn use_attributes(source: &str) -> ImportAttributes {
        let module = parse_test_module(source);
        let Some(Item::Use(use_decl)) = module.items.into_iter().next() else {
            panic!("the source is one `use`");
        };
        use_decl.attributes.expect("the `use` carries a `with`")
    }

    #[test]
    fn a_type_beside_generator_reads_no_wasm_asset() {
        let generated = use_attributes(
            "use { f } from \"./m.wasm\" with { type: \"wasm\", generator: { module: \"lib:gen\" } };",
        );
        assert_eq!(wasm_asset_kind_from_attrs(Some(&generated)), None);

        let asset = use_attributes("use { f } from \"./m.wasm\" with { type: \"wasm\" };");
        assert_eq!(
            wasm_asset_kind_from_attrs(Some(&asset)),
            Some(WasmAssetKind::Wasm)
        );
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

type StdlibSlotMap = hashmap::IndexMap<&'static str, StdlibSlot>;

fn stdlib_slots() -> &'static StdlibSlotMap {
    use std::sync::OnceLock;

    static SLOTS: OnceLock<StdlibSlotMap> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let core = stdlib::all_core_modules();
        let bindings = stdlib::all_binding_modules();
        let mut slots: StdlibSlotMap =
            hashmap::IndexMap::with_capacity_and_hasher(core.len() + bindings.len(), FxBuildHasher);
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
fn wasm_binding_slots() -> &'static hashmap::IndexMap<&'static str, std::sync::OnceLock<Module>> {
    use std::sync::OnceLock;

    static SLOTS: OnceLock<hashmap::IndexMap<&'static str, OnceLock<Module>>> = OnceLock::new();
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
    cm_source_interfaces: SourceInterfaceBatch,
    /// Set of wasm asset namespace keys whose bytes are already in
    /// `wasm_assets` (used for dedup across multiple imports of the same
    /// asset).
    loaded_wasm_namespaces: IndexSet<String>,
    /// Wasm asset imports queued by `load_implicit_modules` (sync method);
    /// drained by `load_all` after that runs so the async fetch can happen.
    pending_implicit_wasm_imports: Vec<(ModuleSource, WasmAssetKind, PathSite)>,
    /// Registry-component imports discovered during `collect_imports`: a
    /// coordinate `use { X } from "ns:pkg"` resolved to an already-materialized
    /// [`ModuleSource::Wasm`] via the dependency index. Drained like
    /// `pending_implicit_wasm_imports`, but from the resolved source directly.
    pending_component_imports: Vec<(ModuleSource, WasmAssetKind, PathSite)>,
    /// Bundled stdlib modules a decoded CM component needs: its host-leaf
    /// packages and whatever its binding module imports. Loaded once every
    /// component import is seen.
    pending_component_stdlib_deps: IndexSet<ModuleSource>,
    /// The entry module source (for dedup when sub-modules import back to entry)
    entry_module_source: Option<ModuleSource>,
    /// Kiln invocation redirects: `(decl_file, from_path)` → generated entry
    /// module path. Consulted by `resolve_import` so a bare `use { X } from
    /// "<schema>"` picks up the generator's output.
    invocations: InvocationIndex,
    /// Set by [`Self::with_generated_bodies_skipped`].
    skip_generated_bodies: bool,
    skipped_bodies: IndexSet<ModuleSource>,
    imports: IndexMap<ModuleSource, Vec<ModuleSource>>,
    asset_origins: IndexMap<String, PathSite>,
    /// Each remote file's pins, in the order its importers were read, each
    /// where its `integrity` was written.
    remote_pins: IndexMap<String, Vec<(Integrity, DiagnosticSpan)>>,
    /// The digest of what each remote file held when it was read, which every
    /// pin is held to once the graph is loaded: an importer read later may pin
    /// a file read earlier.
    remote_reads: IndexMap<String, Integrity>,
}

impl<'a, H: CompilerHost> ModuleLoader<'a, H> {
    /// Create a new module loader with the given host and log level
    pub fn new(host: &'a H, log_level: LogLevel) -> Self {
        let mut interner = ModuleSourceInterner::new();
        interner.set_dependencies(host.dependency_index());
        if host.serves_core() {
            interner.join_core_package();
        }
        Self {
            host,
            log_level,
            logger: Logger::new(host, log_level),
            interner,
            loaded: IndexMap::default(),
            loading: IndexSet::default(),
            implicit_modules: IndexSet::default(),
            wasm_assets: IndexMap::default(),
            cm_source_interfaces: SourceInterfaceBatch::default(),
            loaded_wasm_namespaces: IndexSet::default(),
            pending_implicit_wasm_imports: Vec::new(),
            pending_component_imports: Vec::new(),
            pending_component_stdlib_deps: IndexSet::default(),
            entry_module_source: None,
            invocations: InvocationIndex::new(),
            skip_generated_bodies: false,
            skipped_bodies: IndexSet::default(),
            imports: IndexMap::default(),
            asset_origins: IndexMap::default(),
            remote_pins: IndexMap::default(),
            remote_reads: IndexMap::default(),
        }
    }

    /// Skip the function bodies of every `#![generated]` user module that
    /// imports none of this package's code, for a caller that walks no body
    /// such a module holds ([`LoadResult::skipped_bodies`] names them).
    #[must_use]
    pub(crate) fn with_generated_bodies_skipped(mut self) -> Self {
        self.skip_generated_bodies = true;
        self
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
    pub fn with_invocations(mut self, invocations: InvocationIndex) -> Self {
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
        self.load_all_inner(entry_ast, tentative_entry_source).await
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
        self.load_all_inner(entry_ast, tentative_entry_source).await
    }

    /// Shared post-parse loading: `tentative_entry_source` is the
    /// interned `EntryPoint` [`ModuleSource`] pre-resolved by the caller,
    /// so neither public entry point hits the interner twice.
    async fn load_all_inner(
        mut self,
        entry_ast: Module,
        tentative_entry_source: ModuleSource,
    ) -> Result<LoadResult, LoadError> {
        let entry_module_source = parse_stdlib_identity_attribute(
            &mut self.interner,
            &entry_ast,
            &tentative_entry_source.source_path(),
        )?
        .unwrap_or(tentative_entry_source);
        if let ModuleSource::EntryPoint { filename, .. } = &entry_module_source {
            let entry = canonicalize_entry_point(filename);
            self.interner.bound_packages(&entry, self.host);
        }
        self.entry_module_source = Some(entry_module_source.clone());

        let entry_name = entry_module_source.to_string();
        self.logger.span_start(&format!("load {entry_name}"));

        // Collect imports from entry module (before bind)
        let mut pending: VecDeque<(ModuleSource, PathSite)> = VecDeque::new();
        let mut wasm_imports: Vec<(ModuleSource, WasmAssetKind, PathSite)> = Vec::new();
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

        let mut unpinned = VecDeque::new();
        while let Some((module_source, site)) = next_in_pin_order(
            &mut pending,
            &mut unpinned,
            |(s, _)| matches!(s, ModuleSource::Remote { url, .. } if !self.remote_pins.contains_key(url.as_str())),
        ) {
            // Skip if already loaded
            if self.loaded.contains_key(&module_source) {
                continue;
            }

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
            let source = self
                .get_source(&module_source, &site.written)
                .await
                .map_err(|error| error.at_path(site.at))?;
            let skip_generated_bodies =
                self.skip_generated_bodies && module_source.is_user_authored();
            let (mut ast, skipped) = {
                let _span = self.logger.span(&format!("parse {mod_name}"));
                self.parse_source_with(&source, &module_source, skip_generated_bodies)?
            };

            // Collect its imports (before bind)
            self.collect_imports(&ast, &module_source, &mut pending, &mut wasm_imports)?;
            // Another package's code never imports this package's, so a module
            // importing only it and the standard library reaches none of this
            // package's code, and no caller walks its bodies. One importing
            // this package's code may be walked, and keeps them.
            if let Some(parser) = skipped {
                let reaches_package = |imported: &ModuleSource| {
                    imported.is_user_authored()
                        && !matches!(
                            imported,
                            ModuleSource::Dependency { .. } | ModuleSource::Remote { .. }
                        )
                };
                if self
                    .imports
                    .get(&module_source)
                    .is_some_and(|imported| imported.iter().any(reaches_package))
                {
                    Self::read_skipped_bodies(parser, &mut ast, &module_source)?;
                } else {
                    self.skipped_bodies.insert(module_source.clone());
                }
            }

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
        for (source, kind, site) in wasm_imports {
            self.handle_wasm_source(source, kind, site).await?;
        }

        // Load implicit modules (for compiler-generated code)
        self.logger.span_start("load/implicit_modules");
        let implicit_result = self.load_implicit_modules();
        self.logger.span_end("load/implicit_modules");
        implicit_result?;

        // Drain any wasm asset imports surfaced by implicit modules (e.g.
        // `core:builtin` declaring `use _ from "./libm.wat" with { type: "wat" };`).
        let queued = std::mem::take(&mut self.pending_implicit_wasm_imports);
        for (source, kind, site) in queued {
            self.handle_wasm_source(source, kind, site).await?;
        }

        // Load registry-component dependencies resolved from a coordinate `use`
        // (already-materialized `.wasm` paths from the dependency index).
        let components = std::mem::take(&mut self.pending_component_imports);
        for (source, kind, site) in components {
            self.handle_wasm_source(source, kind, site).await?;
        }

        // Now that every component import (file-path and registry-coordinate)
        // has been seen, load the stdlib modules they need.
        self.load_pending_component_stdlib_deps();
        let queued = std::mem::take(&mut self.pending_implicit_wasm_imports);
        for (source, kind, site) in queued {
            self.handle_wasm_source(source, kind, site).await?;
        }

        self.check_remote_pins()?;
        self.check_stdlib_identities()?;
        for import_path in self.loaded.keys().filter_map(stdlib_cache_key) {
            self.host.stdlib_module_read(&import_path);
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
            cm_source_interfaces: self.cm_source_interfaces,
            invocations: self.invocations,
            interner: self.interner,
            skipped_bodies: self.skipped_bodies,
            imports: self.imports,
            asset_origins: self.asset_origins,
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
    /// can run their async loading after this synchronous walk, by when every
    /// pin a `use` writes is noted.
    fn collect_imports(
        &mut self,
        module: &Module,
        from_module_source: &ModuleSource,
        pending: &mut VecDeque<(ModuleSource, PathSite)>,
        wasm_imports_out: &mut Vec<(ModuleSource, WasmAssetKind, PathSite)>,
    ) -> Result<(), LoadError> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let site = use_site(from_module_source, use_decl);
                if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()) {
                    let entry_dir = entry_dir_of(self.entry_module_source.as_ref());
                    let path =
                        resolve_wasm_asset_path(from_module_source, &use_decl.source, &entry_dir)
                            .map_err(|error| error.at_path(site.at.clone()))?;
                    if is_remote(&path) {
                        self.note_remote(&path, use_decl, from_module_source, &site)?;
                    }
                    wasm_imports_out.push((self.interner.wasm(&path, kind), kind, site));
                    continue;
                }
                let resolved = self
                    .resolve_import(from_module_source, &use_decl.source)
                    .map_err(|error| error.at_path(site.at.clone()))?;
                // A coordinate resolving to a prebuilt component (registry
                // dependency) loads across the CM boundary, not as Wado source.
                if let ModuleSource::Wasm { kind, .. } = &resolved {
                    let kind = *kind;
                    self.pending_component_imports.push((resolved, kind, site));
                    continue;
                }
                if let ModuleSource::Remote { url, .. } = &resolved {
                    self.note_remote(url, use_decl, from_module_source, &site)?;
                }
                let declares_generator = use_decl
                    .attributes
                    .as_ref()
                    .is_some_and(ImportAttributes::is_generated);
                if declares_generator && !matches!(resolved, ModuleSource::Redirected { .. }) {
                    // No invocation produced this module. Reading the schema as
                    // Wado would report the generator's absence as a parse
                    // error in a file that was never Wado.
                    self.emit_kiln_no_generated_module(from_module_source, use_decl);
                    continue;
                }
                // A `type` no reader takes names a file that is not Wado, which
                // analysis reports on the use site.
                if !declares_generator
                    && use_decl
                        .attributes
                        .as_ref()
                        .is_some_and(|attrs| !attrs.reads_as_source())
                {
                    continue;
                }
                self.imports
                    .entry(from_module_source.clone())
                    .or_default()
                    .push(resolved.clone());
                pending.push_back((resolved, site));
            }
        }
        Ok(())
    }

    /// Check the remote `url` a `use` names, and keep the pin it writes for
    /// [`Self::check_remote_pins`].
    fn note_remote(
        &mut self,
        url: &str,
        use_decl: &UseDecl,
        from: &ModuleSource,
        site: &PathSite,
    ) -> Result<(), LoadError> {
        if let Some(reason) = url_fault(url) {
            return Err(LoadError::InvalidRemoteUrl {
                url: url.to_string(),
                reason,
            }
            .at_path(site.at.clone()));
        }
        if let Some(pin) = written_pin(use_decl.attributes.as_ref(), from)? {
            self.remote_pins
                .entry(url.to_string())
                .or_default()
                .push(pin);
        }
        Ok(())
    }

    /// Load a wasm asset from a resolved [`ModuleSource::Wasm`]: one a
    /// `with { type: "wat"|"wasm" }` clause names, or a registry component
    /// dependency whose `.wasm` path is resolved from the dependency index. A
    /// diagnostic in its bindings is reported at the first `site` importing it.
    async fn handle_wasm_source(
        &mut self,
        source: ModuleSource,
        kind: WasmAssetKind,
        site: PathSite,
    ) -> Result<(), LoadError> {
        self.asset_origins
            .entry(source.source_path())
            .or_insert_with(|| site.clone());
        self.load_wasm_source(source, kind, &site.written)
            .await
            .map_err(|error| error.at_path(site.at))
    }

    async fn load_wasm_source(
        &mut self,
        source: ModuleSource,
        kind: WasmAssetKind,
        written: &str,
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
        let span_name = format!("load_wasm_asset {namespace}");
        self.logger.span_start(&span_name);
        let fetched = self.fetch_wasm_asset_bytes(&source, &path, written).await;
        self.logger.span_end(&span_name);
        let raw_bytes = fetched?;

        let core_wasm_bytes = match kind {
            WasmAssetKind::Wat => wat::parse_bytes(&raw_bytes)
                .map_err(|e| LoadError::WasmImport {
                    module_source: source.clone(),
                    message: format!("failed to parse .wat: {e}"),
                })?
                .into_owned(),
            WasmAssetKind::Wasm => raw_bytes,
        };

        // A component takes the component path, a core module falls through. The
        // text form says which just as the binary does, so the bytes decide.
        if is_wasm_component(&core_wasm_bytes) {
            return self.handle_component_import(&source, &namespace, core_wasm_bytes);
        }

        let function_exports = parse_wasm_module_exports(&source, &core_wasm_bytes)?;

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
                component_interfaces: Vec::new(),
                imported_functions: Vec::new(),
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
                self.pending_component_stdlib_deps.insert(ms);
            }
        }

        let mut binding_imports = VecDeque::new();
        let mut binding_wasm_imports = Vec::new();
        self.collect_imports(
            &bindings.module,
            source,
            &mut binding_imports,
            &mut binding_wasm_imports,
        )?;
        assert!(
            binding_wasm_imports.is_empty(),
            "a component binding module imports no wasm asset"
        );
        self.pending_component_stdlib_deps
            .extend(binding_imports.into_iter().map(|(ms, _)| ms));

        self.cm_source_interfaces.extend(bindings.source_interfaces);
        self.bind_module(&bindings.module, source)?;
        self.loaded.insert(source.clone(), bindings.module);
        self.loaded_wasm_namespaces.insert(namespace.to_string());
        self.wasm_assets.insert(
            namespace.to_string(),
            WasmAsset {
                bytes,
                function_exports: Vec::new(),
                component_interfaces: bindings.interfaces,
                imported_functions: bindings.imported_functions,
            },
        );
        Ok(())
    }

    fn build_component_bindings(
        &self,
        source: &ModuleSource,
        bytes: &[u8],
    ) -> Result<ComponentBindings, LoadError> {
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
        build_bindings(&resolve, world, source).map_err(err)
    }

    /// Fetch the raw bytes of a wasm asset by canonical path. Stdlib paths
    /// (`core:libm.wat`, etc.) hit `stdlib::get_stdlib_wasm_asset`, a URL
    /// [`Self::load_pinned`], and other user paths (`./foo.wat`)
    /// `host.load_source`.
    async fn fetch_wasm_asset_bytes(
        &mut self,
        source: &ModuleSource,
        path: &str,
        written: &str,
    ) -> Result<Vec<u8>, LoadError> {
        if is_bundled_specifier(path) {
            return stdlib::get_stdlib_wasm_asset(path)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| LoadError::ModuleNotFound {
                    path: source.to_string(),
                });
        }
        if is_remote(path) {
            return self.load_pinned(path).await;
        }
        self.host
            .load_source(path)
            .await
            .map_err(|e| LoadError::reading(e, read_name(written, path)))
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

    /// Load the stdlib modules imported components need. Runs after every
    /// component import, file-path and registry-coordinate alike, is processed.
    fn load_pending_component_stdlib_deps(&mut self) {
        let sources: Vec<ModuleSource> = std::mem::take(&mut self.pending_component_stdlib_deps)
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

                while let Some((dep_ms, _)) = pending.pop_front() {
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

    /// Report a `use ... from "<schema>"` that names a generator no invocation
    /// produced a module for.
    fn emit_kiln_no_generated_module(&self, from_module_source: &ModuleSource, use_decl: &UseDecl) {
        let file = decl_file_of(from_module_source);
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code: Code::KilnNoGeneratedModule,
            message: format!(
                "kiln: no generated module for `use ... from {:?}` — its generator \
                 declared no output for this schema, or ran under a different name \
                 than the one this module imports",
                use_decl.source.value,
            ),
            span: Some(DiagnosticSpan::from_span(&use_decl.source_span, Some(file))),
        });
    }

    /// Resolve an import source relative to the importing module, or say why
    /// a bare one names nothing.
    fn resolve_import(
        &mut self,
        from_module_source: &ModuleSource,
        import_source: &str,
    ) -> Result<ModuleSource, LoadError> {
        let resolved = resolve_import_with_invocations(
            &mut self.interner,
            from_module_source,
            import_source,
            self.entry_module_source.as_ref(),
            &self.invocations,
        );
        let ModuleSource::Local { path, .. } = &resolved else {
            return Ok(resolved);
        };
        if is_cwd_relative(import_source) {
            if !resolved.is_user_authored() {
                return Ok(resolved);
            }
            let path = path.to_string();
            return Ok(self.interner.place(&path, self.host));
        }
        // A dependency of a package a path reached first, placed in it.
        if self.interner.is_placed(path) {
            return Ok(resolved);
        }

        let exported_file = split_exported_file(import_source);
        if let Some((package, file)) = exported_file
            && let Some(Err(refusal)) =
                self.interner
                    .resolve_exported_file(from_module_source, package, file)
        {
            return Err(LoadError::UnexportedFile {
                package: package.to_string(),
                file: file.to_string(),
                refusal,
            });
        }
        let name = exported_file.map_or(import_source, |(package, _)| package);

        // Declared but unresolvable (e.g. missing `[package].lib`): report
        // why, instead of a generic "invalid module path".
        if let Some(reason) = self
            .interner
            .unresolved_dependency(from_module_source, name)
        {
            return Err(LoadError::DependencyUnresolved {
                name: name.to_string(),
                reason: reason.to_string(),
            });
        }
        let manifest = self.interner.dependency_manifest(from_module_source);
        if let Some(DependencyManifest {
            path,
            error: Some(error),
        }) = manifest
        {
            return Err(LoadError::DependencyUnresolved {
                name: name.to_string(),
                reason: format!("{path} is invalid: {error}"),
            });
        }

        // A namespaced import (xxx:yyy) names a dependency
        if let Some((namespace, _)) = name.split_once(':')
            && is_package_namespace(namespace)
        {
            return Err(LoadError::UndeclaredDependency {
                name: name.to_string(),
                manifest: manifest.map(|m| m.path.clone()),
            });
        }

        // Invalid module path (no recognized prefix)
        Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        })
    }

    /// Get source code for a module
    async fn get_source(
        &mut self,
        module_source: &ModuleSource,
        written: &str,
    ) -> Result<String, LoadError> {
        match module_source {
            ModuleSource::Local {
                path: host_path, ..
            }
            | ModuleSource::Dependency {
                path: host_path, ..
            } => {
                self.read_text(host_path, read_name(written, host_path))
                    .await
            }
            ModuleSource::Remote { url, .. } => utf8_text(self.load_pinned(url).await?, url),
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
            ModuleSource::Effect { .. } => unreachable!("an effect namespace is never loaded"),
            ModuleSource::Redirected { uri, .. } => {
                // Strip the `file:` scheme so the host sees a plain
                // absolute path. Other schemes are passed through
                // unchanged so in-memory hosts can use the URI as a key.
                let host_path = strip_kiln_scheme(uri).unwrap_or_else(|| uri.to_string());
                // The generated module is the file read, not the schema the
                // `use` names.
                self.read_text(&host_path, &host_path).await
            }
        }
    }

    /// Read `host_path` as UTF-8 text, naming it `name` if that fails.
    async fn read_text(&self, host_path: &str, name: &str) -> Result<String, LoadError> {
        let bytes = self
            .host
            .load_source(host_path)
            .await
            .map_err(|e| LoadError::reading(e, name))?;
        utf8_text(bytes, name)
    }

    /// The remote file `url`, held by the host to the first pin a `use` of it
    /// wrote, or to the lock's; [`Self::check_remote_pins`] holds it to the
    /// rest.
    async fn load_pinned(&mut self, url: &str) -> Result<Vec<u8>, LoadError> {
        let pin = self
            .remote_pins
            .get(url)
            .and_then(|pins| pins.first())
            .map(|(pin, _)| *pin);
        let bytes = self
            .host
            .load_remote(url, None, pin)
            .await
            .map_err(|e| LoadError::reading(e, url))?;
        self.remote_reads
            .insert(url.to_string(), Integrity::of(&bytes));
        Ok(bytes)
    }

    /// Hold every remote file read to every pin a `use` of it wrote, each
    /// reported where it was written.
    fn check_remote_pins(&self) -> Result<(), LoadError> {
        for (url, pins) in &self.remote_pins {
            let Some(read) = self.remote_reads.get(url) else {
                continue;
            };
            for (pin, at) in pins {
                check_digest(url, *read, *pin, PinnedBy::Use)
                    .map_err(|e| LoadError::reading(e, url).at_path(at.clone()))?;
            }
        }
        Ok(())
    }

    /// Parse source code into a module AST
    fn parse_source(
        &self,
        source: &str,
        module_source: &ModuleSource,
    ) -> Result<Module, LoadError> {
        Ok(self.parse_source_with(source, module_source, false)?.0)
    }

    /// Parse `source`, skipping a generated module's function bodies when
    /// `skip_generated_bodies`. Where they were skipped, the parser comes back
    /// to read them on demand.
    fn parse_source_with(
        &self,
        source: &str,
        module_source: &ModuleSource,
        skip_generated_bodies: bool,
    ) -> Result<(Module, Option<Parser>), LoadError> {
        let lex_result = lex(source);
        if let Some(e) = lex_result.errors.first() {
            return Err(LoadError::from_lex_error(e, module_source.clone()));
        }

        let mut parser = Parser::from_lex_no_trivia(lex_result);
        if skip_generated_bodies {
            parser = parser.skipping_generated_bodies();
        }
        // Batch loading is fail-fast: report the first recovered syntax error
        // as a load error so compilation never proceeds on a partial AST.
        let ast = parser.parse();
        if let Some(e) = parser.take_errors().first() {
            return Err(LoadError::from_parse_error(e, module_source.clone()));
        }
        Ok((ast, parser.skipped_bodies().then_some(parser)))
    }

    /// Read the bodies `parser` skipped into `ast`, fail-fast as a parse is.
    fn read_skipped_bodies(
        mut parser: Parser,
        ast: &mut Module,
        module_source: &ModuleSource,
    ) -> Result<(), LoadError> {
        parser.read_skipped_bodies(ast);
        match parser.take_errors().first() {
            Some(e) => Err(LoadError::from_parse_error(e, module_source.clone())),
            None => Ok(()),
        }
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
        let mut pairs: IndexMap<[String; 2], DiagnosticSpan> = IndexMap::default();
        let mut resolve_path: IndexMap<String, String> = IndexMap::default();
        for (module_source, module) in &self.loaded {
            let ms_str = module_source.to_string();
            for (raw_path, span) in module.include_paths() {
                let module_path = resolve_path
                    .entry(ms_str.clone())
                    .or_insert_with(|| module_source.source_path());
                pairs
                    .entry([ms_str.clone(), raw_path.clone()])
                    .or_insert_with(|| DiagnosticSpan::from_span(span, Some(module_path)));
            }
        }
        // Format the entry module source once so the per-include elaborator
        // can compare against it without allocating per call.
        let entry_module_source = self
            .entry_module_source
            .as_ref()
            .map(ModuleSource::source_path);
        let mut included = IndexMap::default();
        for (pair, at) in pairs {
            let [ref ms_str, ref raw_path] = pair;
            // Only a relative path names the file beside the module, which is
            // also what keeps a remote module off this machine.
            if !is_cwd_relative(raw_path) {
                return Err(LoadError::InvalidIncludePath {
                    path: raw_path.clone(),
                }
                .at_path(at));
            }
            // Resolve path relative to the module source's real directory.
            let module_path = resolve_path
                .get(ms_str)
                .map_or(ms_str.as_str(), String::as_str);
            let resolved =
                resolve_include_path_impl(entry_module_source.as_deref(), module_path, raw_path);
            let bytes = self
                .host
                .load_file(&resolved, None, None)
                .await
                .map_err(|e| LoadError::reading_include(e, raw_path).at_path(at))?;
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
    assert!(is_cwd_relative(raw_path), "an include path is relative");
    if is_remote(module_source_str) {
        return resolve_module_path(module_source_str, raw_path);
    }
    let stripped = raw_path.strip_prefix("./").unwrap_or(raw_path);
    if let Some(dir) = dir_prefix_to_keep(entry_module_source, module_source_str) {
        format!("{dir}/{stripped}")
    } else {
        stripped.to_string()
    }
}

/// The directory an include path keeps as its prefix: an absolute or `./`/`../`
/// module's, except a cwd-relative entry's, which `base_path.join` already adds.
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
    (module_source_str.starts_with('/') || is_cwd_relative(module_source_str)).then_some(dir)
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
    fn import_at_base_root_stays_at_base_root() {
        let entry = "./main.wado";
        let import = "./helper.wado";
        let resolved = resolve(Some(entry), import, "./data.txt");
        assert_eq!(resolved, "./data.txt");
    }

    #[test]
    fn parent_relative_import_keeps_dir_prefix() {
        let entry = "./main.wado";
        let import = "../sibling/helper.wado";
        let resolved = resolve(Some(entry), import, "./data.txt");
        assert_eq!(resolved, "../sibling/data.txt");
    }

    #[test]
    fn import_from_the_parent_dir_keeps_its_dir() {
        let entry = "./main.wado";
        let import = "../helper.wado";
        let resolved = resolve(Some(entry), import, "../data.txt");
        assert_eq!(resolved, "../../data.txt");
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
}
