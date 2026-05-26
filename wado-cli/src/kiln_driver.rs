//! Kiln pipeline driver — plan computation, generator execution, and cache
//! bookkeeping for inline-declared invocations.
//!
//! All Kiln invocations originate from inline `use ... with { generator: ... }`
//! clauses; the manifest does not declare any. This module is the CLI-side
//! glue between the parsed inline invocation set and the pure-data
//! algorithmic module at [`wado_compiler::kiln`]. Its responsibilities are:
//!
//! 1. Topologically sort the inline invocation set via [`build_plan`].
//! 2. Execute a single invocation by calling
//!    [`wado_compiler::compiler_host::CompilerHost::run_generator`] and
//!    writing the response to disk under the invocation's output
//!    directory, with a canonical `#![generated]` header stamped on each
//!    file — see [`execute`].
//! 3. Convert an [`InvocationRun`] into a [`crate::kiln_metadata::Metadata`]
//!    record for persistence (`build_metadata`), check a recorded
//!    `<primary>.kiln.json` for freshness against the current filesystem +
//!    sources (`cache_matches`), and delete orphaned `#![generated]`
//!    files left over from an earlier run (`reconcile_outputs`).
//!
//! Per-invocation cache state lives at
//! `<manifest_root>/<output_dir>/<primary>.kiln.json` (see
//! [`crate::kiln_metadata`]). `wado.lock` is dependency-pin-only since
//! WEP M9 — it does not contain any generator-cache rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wado_compiler::ast::AttrValue;
use wado_compiler::compiler_host::{
    CompilerHost, GeneratorInputFile, GeneratorReadRecord, GeneratorRequest, GeneratorRunnerError,
    SourceError,
};
#[cfg(test)]
use wado_compiler::kiln::DeclSite;
use wado_compiler::kiln::{
    FileHash, GeneratedHeader, GeneratorModule, Invocation, InvocationPath, OptionsDescriptor,
    Plan, PlanError, content_hash, encode_options_canonical, file_hash, generator_identity,
    has_generated_marker, hex_digest, validate_options,
};
use wado_compiler::{Code, Diagnostic, Severity};
use wado_manifest::Manifest;

use crate::kiln_metadata::{
    self, FileHash as MetaFileHash, METADATA_VERSION, Metadata, OutputEntry as MetaOutputEntry,
};

/// Outcome of plan construction.
#[derive(Debug)]
pub struct PlanOutcome {
    /// Scheduled invocations in execution order. Empty when no inline
    /// `with` clauses were collected.
    pub plan: Plan,
    /// The manifest root used to resolve relative paths, preserved so the
    /// caller can read input files without re-deriving it.
    pub manifest_root: PathBuf,
}

/// Errors from the driver. Reuses the compiler-side [`PlanError`] for
/// dedup/cycle diagnostics.
#[derive(Debug)]
pub enum DriverError {
    /// Dedup / cycle error from the planner.
    Plan(PlanError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<PlanError> for DriverError {
    fn from(e: PlanError) -> Self {
        DriverError::Plan(e)
    }
}

/// Outcome of [`execute`] for a single invocation.
///
/// Carries the information downstream layers (cache check, lockfile
/// writer, GC) need: every path that was written to disk, each written
/// file's content hash, and the list of `read-file` calls the generator
/// made during this run.
#[derive(Debug, Clone)]
pub struct InvocationRun {
    /// Content hash of the primary schema at invocation time. Recorded in
    /// the lockfile so subsequent runs can detect input drift.
    pub primary: FileHash,
    /// Content hashes for each declared input, in declaration order.
    pub inputs: Vec<FileHash>,
    /// One entry per output file the generator produced. Paths are
    /// project-root-relative, normalized (`output_dir` joined with the
    /// generator-relative path and forward-slash-only).
    pub outputs: Vec<OutputHash>,
    /// Every `host::read-file` call the generator made, relayed verbatim
    /// from the runner. Fed into the next run's cache key so transitive
    /// reads invalidate the cache when their contents change.
    pub reads: Vec<GeneratorReadRecord>,
}

/// Output-file identity recorded after a generator run.
///
/// Produced by [`execute`], consumed by the cache-check / metadata layer
/// and by `wado check` (which compares `bytes` against on-disk content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputHash {
    /// Project-root-relative forward-slash path of the written file.
    pub path: String,
    /// SHA-256 of the file contents as written (header + generator body).
    pub hash: [u8; 32],
    /// Whether the generator marked this file as the invocation's entry
    /// module — the one a consuming `use ... from "<from>"` resolves to.
    pub is_entry: bool,
    /// Full file bytes (header + generator body) as they would land on
    /// disk. Always populated by [`execute`], regardless of write mode,
    /// so `wado check` can byte-compare against the on-disk file without
    /// re-running the generator.
    pub bytes: Vec<u8>,
}

/// Errors from [`execute`].
#[derive(Debug)]
pub enum ExecuteError {
    /// Failed to load the primary schema or a declared input.
    LoadInput { path: String, source: SourceError },
    /// The runner surfaced an error. `Unsupported` bubbles up here and is
    /// the signal a future consume-only layer (M7) watches for.
    Runner(GeneratorRunnerError),
    /// Filesystem I/O failure while writing a generated file or creating
    /// its parent directory.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A generator output's `path` is empty or attempts to escape the
    /// invocation's output directory (`..` segments, absolute paths).
    InvalidOutputPath { path: String },
}

impl std::fmt::Display for ExecuteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecuteError::LoadInput { path, source } => {
                write!(f, "kiln: failed to load input {path:?}: {source}")
            }
            ExecuteError::Runner(e) => write!(f, "kiln: {e}"),
            ExecuteError::Io { path, source } => {
                write!(f, "kiln: I/O error at {}: {source}", path.display())
            }
            ExecuteError::InvalidOutputPath { path } => {
                write!(f, "kiln: generator produced invalid output path {path:?}")
            }
        }
    }
}

impl std::error::Error for ExecuteError {}

/// Run one invocation: read its inputs, call the host's runner, write
/// the generator's outputs to disk with a `#![generated]` header.
///
/// `component_wasm` is the already-compiled generator component; the
/// caller (higher-level compile flow) is responsible for producing it.
/// `manifest_root` is the project-root directory; paths in `invocation`
/// are interpreted relative to it both for reading inputs and for
/// writing outputs.
///
/// The function does not consult or update `wado.lock`; caching is a
/// separate layer on top of this primitive.
///
/// # Errors
///
/// - [`ExecuteError::LoadInput`] if `host.load_source` fails for the
///   primary or any input.
/// - [`ExecuteError::Runner`] for any error the runner surfaces,
///   including `Unsupported` (which consume-only mode treats specially).
/// - [`ExecuteError::InvalidOutputPath`] if the generator returns a
///   path that escapes the output directory.
/// - [`ExecuteError::Io`] for filesystem failures.
pub async fn execute<H: CompilerHost>(
    invocation: &Invocation,
    component_wasm: &[u8],
    manifest_root: &Path,
    host: &H,
) -> Result<InvocationRun, ExecuteError> {
    execute_with_mode(
        invocation,
        component_wasm,
        manifest_root,
        host,
        ExecuteMode::WriteAndWarnOnOverwrite,
    )
    .await
}

/// Behavior knob for [`execute_with_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteMode {
    /// Default `wado compile` behavior: write generator outputs to disk,
    /// surface a [`Code::KilnGeneratedRegenerated`] warning when the new
    /// bytes differ from the pre-existing on-disk file.
    WriteAndWarnOnOverwrite,
    /// `wado check` behavior: do not write to disk. The caller is
    /// responsible for byte-comparing the returned [`InvocationRun`]
    /// against on-disk files and surfacing
    /// [`Code::KilnGeneratedStaleOnDisk`].
    DryRun,
}

/// Run a generator and optionally write outputs to disk.
///
/// The default `execute` calls this with [`ExecuteMode::WriteAndWarnOnOverwrite`].
pub async fn execute_with_mode<H: CompilerHost>(
    invocation: &Invocation,
    component_wasm: &[u8],
    manifest_root: &Path,
    host: &H,
    mode: ExecuteMode,
) -> Result<InvocationRun, ExecuteError> {
    let primary = load_input(host, &invocation.from).await?;
    let primary_hash = file_hash(&invocation.from, primary.content.as_bytes());
    let mut inputs = Vec::with_capacity(invocation.inputs.len());
    let mut input_hashes = Vec::with_capacity(invocation.inputs.len());
    for p in &invocation.inputs {
        let file = load_input(host, p).await?;
        input_hashes.push(file_hash(p, file.content.as_bytes()));
        inputs.push(file);
    }

    // Canonical options are UTF-8 JSON bytes produced by
    // `encode_options_canonical`. Converting to `String` at the wire
    // boundary upholds the invariant; non-UTF-8 here is a compiler bug.
    let options = String::from_utf8(invocation.options_canonical.clone())
        .expect("kiln: canonical options must be UTF-8");
    let request = GeneratorRequest {
        primary,
        inputs,
        options,
    };

    let response = host
        .run_generator(component_wasm, request)
        .await
        .map_err(ExecuteError::Runner)?;

    let output_dir_abs = manifest_root.join(invocation.output_dir.as_str());
    let by = generator_identity(&invocation.module);
    let mut sources: Vec<&InvocationPath> = Vec::with_capacity(1 + invocation.inputs.len());
    sources.push(&invocation.from);
    for p in &invocation.inputs {
        sources.push(p);
    }
    let source_paths: Vec<InvocationPath> = sources.iter().map(|p| (*p).clone()).collect();
    let header = GeneratedHeader::emit_with_paths(&by, &source_paths);

    let mut outputs = Vec::with_capacity(response.files.len());
    for file in &response.files {
        let rel = validate_rel_output_path(&file.path)?;
        let full_path = output_dir_abs.join(&rel);
        let mut bytes = Vec::with_capacity(header.len() + file.content.len());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(file.content.as_bytes());

        let normalized = InvocationPath::normalize(&format!(
            "{}/{}",
            invocation.output_dir.as_str(),
            rel.to_string_lossy()
        ));

        match mode {
            ExecuteMode::WriteAndWarnOnOverwrite => {
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| ExecuteError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                if let Ok(existing) = std::fs::read(&full_path)
                    && existing != bytes
                {
                    emit_generated_regenerated_warning(
                        host,
                        &invocation.decl_site.synthetic_id,
                        normalized.as_str(),
                    );
                }
                std::fs::write(&full_path, &bytes).map_err(|source| ExecuteError::Io {
                    path: full_path.clone(),
                    source,
                })?;
            }
            ExecuteMode::DryRun => {
                // Skip directory creation and write; caller (wado check)
                // compares `bytes` against the on-disk file itself.
            }
        }

        let hash = file_hash(&normalized, &bytes).hash;
        outputs.push(OutputHash {
            path: normalized.as_str().to_string(),
            hash,
            is_entry: file.is_entry,
            bytes,
        });
    }

    Ok(InvocationRun {
        primary: primary_hash,
        inputs: input_hashes,
        outputs,
        reads: response.reads,
    })
}

/// Convert an [`InvocationRun`] into a [`Metadata`] record ready to be
/// written to `<output_dir>/<primary>.kiln.json`.
///
/// `invocation_name` is the synthesized id derived from `(decl_file,
/// from)` for inline clauses; it is recorded in the metadata's own
/// `invocation` field for diagnostics. `options_hash` is the hex
/// SHA-256 of the canonical options encoding — produced by
/// [`wado_compiler::kiln::hash_options_canonical`] so it stays stable
/// across the M3 provisional encoder and the M4 lifted-form encoder.
///
/// Sorts the `reads` list lexicographically by path, mirroring what
/// [`cache_matches`] expects on the next run.
#[must_use]
pub fn build_metadata(
    invocation_name: &str,
    invocation: &Invocation,
    run: &InvocationRun,
    options_hash: String,
    generator_source_hash: String,
) -> Metadata {
    let generator = generator_identity(&invocation.module);
    let primary = to_meta_file_hash(&run.primary);
    let inputs: Vec<MetaFileHash> = run.inputs.iter().map(to_meta_file_hash).collect();

    let mut reads: Vec<MetaFileHash> = run
        .reads
        .iter()
        .map(|r| MetaFileHash {
            path: InvocationPath::normalize(&r.path).as_str().to_string(),
            hash: hex_digest(&r.content_hash),
        })
        .collect();
    reads.sort_by(|a, b| a.path.cmp(&b.path));
    reads.dedup_by(|a, b| a.path == b.path);

    let outputs: Vec<MetaOutputEntry> = run
        .outputs
        .iter()
        .map(|o| MetaOutputEntry {
            path: o.path.clone(),
            hash: hex_digest(&o.hash),
            entry: o.is_entry,
        })
        .collect();

    Metadata {
        version: METADATA_VERSION,
        invocation: invocation_name.to_string(),
        generator,
        generator_source_hash,
        primary,
        inputs,
        options_hash,
        reads,
        outputs,
    }
}

fn to_meta_file_hash(f: &FileHash) -> MetaFileHash {
    MetaFileHash {
        path: f.path.clone(),
        hash: hex_digest(&f.hash),
    }
}

/// Compose a `file:` URI from an absolute filesystem path.
///
/// Used to populate the [`wado_compiler::kiln::InvocationIndex`] entries
/// the loader consults at module-resolve time.
///
/// Uses the `kiln:` scheme without authority (`kiln:/abs/path`) rather
/// than `file://` because the compiler's qualified-name format
/// (`{module_source}//{name}`) treats `//` as the separator between
/// module source and symbol name. A `file:///abs/path` URI contains its
/// own `//` and confuses every parser that splits on `//` — see
/// `wir_build::types::sort_types_topologically` and the equivalent
/// keying in `register_struct`. The `kiln:/path` form has no internal
/// `//`, so the qualified-name boundary stays unambiguous.
///
/// The URI is RFC 3986–valid (single-segment scheme followed by an
/// absolute-path reference), so `fluent_uri::UriRef::parse` accepts it
/// and the loader's `strip_kiln_scheme` recovers the absolute path.
/// Relative paths are not supported; the caller must canonicalize
/// first.
fn path_to_kiln_uri(path: &Path) -> String {
    // `Path::display` is sufficient on Unix where every absolute path is
    // a `/`-separated UTF-8 string. The CLI is host-only, so any
    // platform-specific path quirk is the kiln_driver's problem to
    // solve, not the wasm32-compatible compiler crate's.
    let s = path.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("kiln:{s}")
    } else {
        // Relative-path fallback. The leading `/` keeps the URI
        // RFC 3986–valid even though the loader will fail to find the
        // file at runtime — a useful diagnostic shape for callers that
        // forgot to canonicalize.
        format!("kiln:/{s}")
    }
}

/// Outcome of comparing a [`Metadata`] record to the current state of
/// the filesystem. See [`cache_matches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheCheck {
    /// Cache key matches and every output file's bytes match the recorded
    /// hash. The generator can be skipped silently.
    Hit,
    /// Cache key matches, but at least one output file's on-disk bytes
    /// differ from the recorded hash — the user has hand-edited the file.
    /// The generator is still skipped (the edit is honored), but a
    /// `Code::KilnGeneratedModified` warning has been emitted.
    HitButModified,
    /// Cache key does not match (or a referenced file is missing). The
    /// generator must run.
    Miss,
}

/// Check whether a recorded `<primary>.kiln.json` is still valid for the
/// given invocation.
///
/// Re-hashes the primary + declared inputs via `host.load_source`, then
/// re-hashes every `reads` entry the same way, then re-hashes every output
/// file from disk (via `manifest_root` joined with the recorded output
/// path). Returns:
///
/// - [`CacheCheck::Hit`] when every hash matches.
/// - [`CacheCheck::HitButModified`] when the cache key matches but an
///   output file's on-disk bytes differ from `metadata.outputs[].hash`.
///   The user has hand-edited a generated file; honor the edit and skip
///   the generator. A `Code::KilnGeneratedModified` warning is emitted
///   for each modified file before returning.
/// - [`CacheCheck::Miss`] when an input/read hash drifted, or an output
///   file is missing/unreadable.
///
/// Load or read failures on inputs are treated as a cache miss; on outputs
/// the missing file produces a brief log warning before returning miss,
/// since the next step (`execute`) will regenerate it anyway.
pub async fn cache_matches<H: CompilerHost>(
    metadata: &Metadata,
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
    current_generator_source_hash: &str,
) -> CacheCheck {
    if metadata.primary.path != invocation.from.as_str() {
        return CacheCheck::Miss;
    }
    if !matches_file(host, &invocation.from, &metadata.primary.hash).await {
        return CacheCheck::Miss;
    }
    // Generator source closure must match. An empty `current` means the
    // provider could not compute one (e.g. spec-form generators in v1);
    // we only treat this as a match against an equally-empty record so
    // hashed metadata never silently downgrades to "always hit".
    if metadata.generator_source_hash != current_generator_source_hash {
        return CacheCheck::Miss;
    }

    if metadata.inputs.len() != invocation.inputs.len() {
        return CacheCheck::Miss;
    }
    for (declared, recorded) in invocation.inputs.iter().zip(&metadata.inputs) {
        if declared.as_str() != recorded.path {
            return CacheCheck::Miss;
        }
        if !matches_file(host, declared, &recorded.hash).await {
            return CacheCheck::Miss;
        }
    }

    for read in &metadata.reads {
        let normalized = InvocationPath::normalize(&read.path);
        if !matches_file(host, &normalized, &read.hash).await {
            return CacheCheck::Miss;
        }
    }

    let mut modified_paths: Vec<String> = Vec::new();
    for output in &metadata.outputs {
        let abs = manifest_root.join(&output.path);
        match std::fs::read(&abs) {
            Ok(bytes) => {
                if !hash_matches_bytes(&bytes, &output.hash) {
                    modified_paths.push(output.path.clone());
                }
            }
            Err(source) => {
                emit_cache_io_warning(host, &abs, &source);
                return CacheCheck::Miss;
            }
        }
    }
    if modified_paths.is_empty() {
        CacheCheck::Hit
    } else {
        for path in &modified_paths {
            emit_generated_modified_warning(host, &metadata.invocation, path);
        }
        CacheCheck::HitButModified
    }
}

fn emit_generated_modified_warning<H: CompilerHost>(host: &H, invocation: &str, path: &str) {
    use wado_compiler::{Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: wado_compiler::Code::KilnGeneratedModified,
        message: format!(
            "kiln[{invocation}]: {path} has been modified after generation; \
             the on-disk content is honored, but `wado check` will fail. \
             Run `wado compile` (or delete the file) to regenerate.",
        ),
        span: None,
    });
}

fn emit_generated_regenerated_warning<H: CompilerHost>(host: &H, invocation: &str, path: &str) {
    use wado_compiler::{Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: wado_compiler::Code::KilnGeneratedRegenerated,
        message: format!(
            "kiln[{invocation}]: regenerating {path} (previous content differed from \
             generator output); local edits to this file have been overwritten.",
        ),
        span: None,
    });
}

fn emit_cache_io_warning<H: CompilerHost>(host: &H, path: &Path, source: &std::io::Error) {
    use wado_compiler::{Code, Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::Log,
        message: format!(
            "kiln cache: failed to read {}: {source}; re-running generator",
            path.display(),
        ),
        span: None,
    });
}

async fn matches_file<H: CompilerHost>(
    host: &H,
    path: &InvocationPath,
    expected_hex: &str,
) -> bool {
    match host.load_source(path.as_str()).await {
        Ok(bytes) => hash_matches_bytes(&bytes, expected_hex),
        Err(_) => false,
    }
}

fn hash_matches_bytes(bytes: &[u8], expected_hex: &str) -> bool {
    hex_digest(&content_hash(bytes)) == expected_hex
}

/// Delete stale `#![generated]` files under `output_dir`.
///
/// Walks `output_dir` recursively. For every regular file whose contents
/// open with a `#![generated]` attribute (see [`has_generated_marker`]),
/// the file is deleted unless its project-root-relative path appears in
/// `kept_paths`. Files without the marker — i.e. anything a user may have
/// placed under `output_dir` by hand — are left alone.
///
/// Returns the list of deleted paths (project-root-relative, forward-slash
/// normalized), ready for a human-facing diagnostic.
///
/// `manifest_root` is used only to derive the relative path of each file
/// for the `kept_paths` comparison; `output_dir` must be an absolute path
/// inside `manifest_root`.
///
/// # Errors
/// Propagates `std::io::Error` from directory traversal and file removal.
/// A missing `output_dir` is not an error — returns an empty list.
pub fn reconcile_outputs(
    manifest_root: &Path,
    output_dir: &Path,
    kept_paths: &[String],
) -> std::io::Result<Vec<String>> {
    if !output_dir.exists() {
        return Ok(Vec::new());
    }
    let kept: indexmap::IndexSet<&str> = kept_paths.iter().map(String::as_str).collect();
    let mut deleted = Vec::new();
    walk_and_delete(manifest_root, output_dir, &kept, &mut deleted)?;
    deleted.sort();
    Ok(deleted)
}

fn walk_and_delete(
    manifest_root: &Path,
    dir: &Path,
    kept: &indexmap::IndexSet<&str>,
    deleted: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_and_delete(manifest_root, &path, kept, deleted)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !has_generated_marker(&content) {
            continue;
        }
        let rel = match path.strip_prefix(manifest_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if kept.contains(rel_str.as_str()) {
            continue;
        }
        std::fs::remove_file(&path)?;
        deleted.push(rel_str);
    }
    Ok(())
}

async fn load_input<H: CompilerHost>(
    host: &H,
    path: &InvocationPath,
) -> Result<GeneratorInputFile, ExecuteError> {
    let bytes =
        host.load_source(path.as_str())
            .await
            .map_err(|source| ExecuteError::LoadInput {
                path: path.as_str().to_string(),
                source,
            })?;
    let content = String::from_utf8(bytes).map_err(|e| ExecuteError::LoadInput {
        path: path.as_str().to_string(),
        source: SourceError::IoError {
            path: path.as_str().to_string(),
            message: format!("not UTF-8: {e}"),
        },
    })?;
    Ok(GeneratorInputFile {
        path: path.as_str().to_string(),
        content,
    })
}

/// Validate and convert a generator-supplied relative path into an
/// [`std::path::PathBuf`] suitable for joining onto the output dir.
///
/// Rejects empty paths, absolute paths, and paths containing `..`. Only
/// forward slashes are treated as separators: on Unix, a backslash is a
/// valid filename byte and this function does not interpret it. Generator
/// authors should emit forward-slash paths.
fn validate_rel_output_path(p: &str) -> Result<PathBuf, ExecuteError> {
    if p.is_empty() {
        return Err(ExecuteError::InvalidOutputPath {
            path: p.to_string(),
        });
    }
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        return Err(ExecuteError::InvalidOutputPath {
            path: p.to_string(),
        });
    }
    for comp in candidate.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ExecuteError::InvalidOutputPath {
                    path: p.to_string(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(candidate.to_path_buf())
}

/// Generator artifacts produced once per unique [`GeneratorModule`]
/// per pipeline run, by [`GeneratorProvider::resolve`]. Every
/// downstream phase reads from the resolved bundle rather than
/// re-asking the provider, so on-disk cache reads happen at most once
/// per module per run.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedGenerator {
    pub wasm: Vec<u8>,
    /// `None` when the generator has no `pub struct Options` (or the
    /// provider can't introspect it); [`typed_encode_options`] then
    /// falls back to the provisional TOML encoding.
    pub descriptor: Option<OptionsDescriptor>,
    /// Hex SHA-256 of the generator's transitive `.wado` closure. The
    /// empty string is a valid value (providers that can't compute
    /// one) and is recorded verbatim — so two such generators never
    /// share a kiln-output cache entry by accident.
    pub source_hash: String,
}

/// Resolves a generator module to its artifacts (component wasm,
/// typed options descriptor, source-closure hash).
///
/// The trait is deliberately a single method: [`run_pipeline`] calls
/// it once per unique module up-front (see [`resolve_modules`]) and
/// hands the same `ResolvedGenerator` to every downstream phase, so
/// implementations don't need their own in-memory cache layer.
pub trait GeneratorProvider {
    /// Resolve `module` to its [`ResolvedGenerator`]. Implementations
    /// own whatever on-disk cache they read; nothing above this layer
    /// dedups, so a second call for the same module will redo all the
    /// work this method does.
    fn resolve(
        &self,
        module: &GeneratorModule,
    ) -> impl std::future::Future<Output = Result<ResolvedGenerator, ProviderError>> + Send;
}

/// Error returned by a [`GeneratorProvider`].
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The provider cannot resolve the module (e.g. build-dependency
    /// resolution not yet wired). The message should guide the user.
    Unsupported { message: String },
    /// The provider failed while compiling or reading the generator.
    Internal { message: String },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Unsupported { message } => write!(f, "{message}"),
            ProviderError::Internal { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Summary of a [`run_pipeline`] invocation.
#[derive(Debug, Default, Clone)]
pub struct PipelineOutcome {
    /// Invocation ids whose cache entry was still valid; no generator run
    /// was performed.
    pub cached: Vec<String>,
    /// Invocation ids whose generator was invoked this run.
    pub executed: Vec<String>,
    /// Project-root-relative paths of stale `#![generated]` files deleted
    /// across all invocation output directories.
    pub deleted: Vec<String>,
    /// Invocation ids that fell through to consume-only mode because the
    /// provider or runner reported `Unsupported`. See the consume-only
    /// contract documented on [`run_pipeline`]: when this list is non-empty
    /// the pipeline emits [`wado_compiler::Code::KilnStaleCache`] warnings
    /// and skips lockfile / output-directory writes so committed artifacts
    /// are preserved.
    pub stale: Vec<String>,
    /// Redirect index for the resolver: `(decl_file, from)` → entry path.
    /// Populated from every invocation (cached, executed, or stale) so the
    /// compiler can redirect `use { X } from "<schema>"` to the generated
    /// entry module. Empty when no invocations ran.
    pub invocations: wado_compiler::kiln::InvocationIndex,
}

/// Failure modes from [`run_pipeline`].
#[derive(Debug)]
pub enum PipelineError {
    /// Planning (dedup, cycle, unsupported module form) failed.
    Driver(DriverError),
    /// A generator invocation failed to execute.
    Execute {
        invocation: String,
        source: ExecuteError,
    },
    /// The provider failed to resolve a generator module.
    Provider {
        invocation: String,
        source: ProviderError,
    },
    /// Reading or writing an output directory failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Writing per-invocation `<primary>.kiln.json` failed.
    MetadataSave {
        invocation: String,
        source: kiln_metadata::MetadataError,
    },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineError::Driver(e) => write!(f, "{e}"),
            PipelineError::Execute { invocation, source } => {
                write!(f, "kiln[{invocation}]: {source}")
            }
            PipelineError::Provider { invocation, source } => {
                write!(f, "kiln[{invocation}]: {source}")
            }
            PipelineError::Io { path, source } => {
                write!(f, "kiln: I/O error at {}: {source}", path.display())
            }
            PipelineError::MetadataSave { invocation, source } => {
                write!(f, "kiln[{invocation}]: {source}")
            }
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<DriverError> for PipelineError {
    fn from(e: DriverError) -> Self {
        PipelineError::Driver(e)
    }
}

/// Emit a debug-severity `SpanStart` diagnostic. The CLI host renders
/// these as `[hh:mm:ss.tttt] >> <name>` when `--log-level debug` is set,
/// so timing breakdowns of the Kiln pipeline are visible alongside the
/// existing `parse` / `bind` / `wir_optimize` spans the compiler emits.
fn span_start<H: CompilerHost>(host: &H, name: &str) {
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Debug,
        code: Code::SpanStart,
        message: name.to_string(),
        span: None,
    });
}

fn span_end<H: CompilerHost>(host: &H, name: &str) {
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Debug,
        code: Code::SpanEnd,
        message: name.to_string(),
        span: None,
    });
}

/// RAII guard: emits `SpanStart` on construction, `SpanEnd` on drop.
/// Lets the surrounding function early-return on any error branch
/// without manually pairing every `return` with a `span_end` call —
/// which kept the Kiln driver's `?` flow free and avoided an extra
/// `async fn` wrapper that would otherwise inflate the layout depth
/// of `compile::run` past rustc's default recursion limit.
struct KilnSpan<'a, H: CompilerHost> {
    host: &'a H,
    name: String,
}

impl<'a, H: CompilerHost> KilnSpan<'a, H> {
    fn new(host: &'a H, name: impl Into<String>) -> Self {
        let name = name.into();
        span_start(host, &name);
        Self { host, name }
    }
}

impl<H: CompilerHost> Drop for KilnSpan<'_, H> {
    fn drop(&mut self) {
        span_end(self.host, &self.name);
    }
}

/// Run the full Kiln pipeline for the given inline invocations: resolve
/// every unique generator once, plan → per-invocation cache check →
/// execute on miss → reconcile stale outputs.
///
/// Returns an empty outcome when `inline_invocations` is empty.
///
/// `no_cache` only bypasses on-disk caches — the per-invocation
/// `<primary>.kiln.json` and the per-generator `build/kiln/` artifacts.
/// In-process artifact sharing (the upfront resolve map, the host-side
/// compiled `Component` cache) is unaffected: it would be wrong to
/// recompile or re-instantiate the same wasm twice within a single
/// pipeline run regardless of how on-disk caching is configured.
///
/// # Errors
/// See [`PipelineError`].
pub async fn run_pipeline<H, P>(
    manifest: &Manifest,
    manifest_root: &Path,
    host: &H,
    provider: &P,
    inline_invocations: Vec<wado_compiler::kiln::Invocation>,
    no_cache: bool,
) -> Result<PipelineOutcome, PipelineError>
where
    H: CompilerHost,
    P: GeneratorProvider,
{
    let _pipeline_span = KilnSpan::new(host, "kiln");
    let plan_order =
        wado_compiler::kiln::build_plan(inline_invocations).map_err(DriverError::Plan)?;
    let mut planned = PlanOutcome {
        plan: plan_order,
        manifest_root: manifest_root.to_path_buf(),
    };
    if planned.plan.order.is_empty() {
        return Ok(PipelineOutcome::default());
    }

    let resolved = resolve_modules(&planned.plan.order, provider, host).await;

    {
        let _s = KilnSpan::new(host, "kiln/typed_encode_options");
        typed_encode_options(manifest, &mut planned.plan.order, &resolved, host);
    }

    let mut outcome = PipelineOutcome::default();
    let mut kept_by_dir: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();

    for invocation in &planned.plan.order {
        let invocation_name = invocation_id(invocation);
        let _inv_span = KilnSpan::new(host, format!("kiln/{invocation_name}"));

        // --no-cache: drop the previously-recorded metadata so the
        // invocation always falls through to the run branch below.
        // We still let `run_and_build_metadata` write a fresh sidecar
        // so a subsequent cache-enabled run can hit it.
        let existing = if no_cache {
            None
        } else {
            match kiln_metadata::load(
                manifest_root,
                invocation.output_dir.as_str(),
                invocation.from.as_str(),
            ) {
                Ok(m) => m,
                Err(source) => {
                    emit_metadata_load_warning(host, &invocation_name, &source);
                    None
                }
            }
        };

        let options_hash =
            wado_compiler::kiln::hash_options_canonical(&invocation.options_canonical);

        let component_result: Result<Arc<ResolvedGenerator>, ProviderError> =
            lookup_resolved(&resolved, &invocation.module);

        let (entry, executed) =
            if let Some(prior) = existing.clone().filter(|m| m.options_hash == options_hash) {
                match component_result {
                    Ok(component) => {
                        let check = {
                            let _s = KilnSpan::new(host, "kiln/cache_check");
                            cache_matches(
                                &prior,
                                invocation,
                                manifest_root,
                                host,
                                &component.source_hash,
                            )
                            .await
                        };
                        match check {
                            CacheCheck::Hit | CacheCheck::HitButModified => {
                                outcome.cached.push(invocation_name.clone());
                                (Some(prior), false)
                            }
                            CacheCheck::Miss => {
                                let r = {
                                    let _s = KilnSpan::new(host, "kiln/run");
                                    run_and_build_metadata(
                                        &invocation_name,
                                        invocation,
                                        manifest_root,
                                        host,
                                        component.as_ref(),
                                        options_hash.clone(),
                                    )
                                    .await
                                };
                                match r {
                                    Ok(metadata) => {
                                        outcome.executed.push(invocation_name.clone());
                                        (Some(metadata), true)
                                    }
                                    Err(e) if is_unsupported(&e) => {
                                        emit_stale_warning(host, &invocation_name);
                                        outcome.stale.push(invocation_name.clone());
                                        (Some(prior), false)
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                        }
                    }
                    Err(ProviderError::Unsupported { .. }) => {
                        emit_stale_warning(host, &invocation_name);
                        outcome.stale.push(invocation_name.clone());
                        (Some(prior), false)
                    }
                    Err(source) => {
                        return Err(PipelineError::Provider {
                            invocation: invocation_name.clone(),
                            source,
                        });
                    }
                }
            } else {
                match component_result {
                    Ok(component) => {
                        let r = {
                            let _s = KilnSpan::new(host, "kiln/run");
                            run_and_build_metadata(
                                &invocation_name,
                                invocation,
                                manifest_root,
                                host,
                                component.as_ref(),
                                options_hash,
                            )
                            .await
                        };
                        match r {
                            Ok(metadata) => {
                                outcome.executed.push(invocation_name.clone());
                                (Some(metadata), true)
                            }
                            Err(e) if is_unsupported(&e) => {
                                emit_stale_warning(host, &invocation_name);
                                outcome.stale.push(invocation_name.clone());
                                (existing, false)
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(ProviderError::Unsupported { .. }) => {
                        emit_stale_warning(host, &invocation_name);
                        outcome.stale.push(invocation_name.clone());
                        (existing, false)
                    }
                    Err(source) => {
                        return Err(PipelineError::Provider {
                            invocation: invocation_name.clone(),
                            source,
                        });
                    }
                }
            };

        if let Some(metadata) = entry {
            if executed {
                let dir = invocation.output_dir.as_str().to_string();
                let kept = kept_by_dir.entry(dir).or_default();
                for o in &metadata.outputs {
                    kept.push(o.path.clone());
                }
            }
            if let Some(entry_path) = metadata.outputs.iter().find(|o| o.entry).map(|o| &o.path) {
                let decl_file = invocation.decl_site.module.clone();
                // Compose a `file:` URI from the canonicalized absolute
                // path. Canonicalizing here means the loader does not
                // need to know the manifest root or the importer's
                // working directory at resolve time. Falling back to the
                // un-canonicalized join (rare: only when the file does
                // not yet exist on disk, e.g. a stale-cache path) keeps
                // the URI well-formed enough for diagnostics.
                let joined = manifest_root.join(entry_path);
                let abs = std::fs::canonicalize(&joined).unwrap_or(joined);
                let uri = path_to_kiln_uri(&abs);
                outcome
                    .invocations
                    .insert(&decl_file, invocation.from.as_str(), &uri);
            }
            if executed
                && let Err(source) = kiln_metadata::save(
                    manifest_root,
                    invocation.output_dir.as_str(),
                    invocation.from.as_str(),
                    &metadata,
                )
            {
                return Err(PipelineError::MetadataSave {
                    invocation: invocation_name.clone(),
                    source,
                });
            }
        }
    }

    if !outcome.stale.is_empty() {
        return Ok(outcome);
    }

    for (dir, kept) in &kept_by_dir {
        let abs = manifest_root.join(dir);
        let deleted =
            reconcile_outputs(manifest_root, &abs, kept).map_err(|source| PipelineError::Io {
                path: abs.clone(),
                source,
            })?;
        outcome.deleted.extend(deleted);
    }
    outcome.deleted.sort();

    Ok(outcome)
}

/// Outcome of [`check_pipeline`].
///
/// `stale.len()` is non-zero iff at least one invocation produced bytes
/// that did not match the on-disk source. Each entry is the
/// project-root-relative path of the divergent file.
#[derive(Debug, Default)]
pub struct CheckOutcome {
    pub checked: Vec<String>,
    pub stale: Vec<String>,
    pub missing: Vec<String>,
    /// Redirect index for the resolver, populated identically to
    /// [`PipelineOutcome::invocations`] so the downstream compile can
    /// resolve `use { ... } from "<schema>"` even though `check_pipeline`
    /// did not write outputs to disk.
    pub invocations: wado_compiler::kiln::InvocationIndex,
}

/// Run the Kiln pipeline in `wado check` mode: re-run every invocation
/// from scratch (ignoring `<primary>.kiln.json`), byte-compare each
/// output against the on-disk file, and surface
/// [`Code::KilnGeneratedStaleOnDisk`] diagnostics for mismatches.
///
/// Does not write generator outputs to disk and does not touch
/// `<primary>.kiln.json`. Suitable for CI: a clean checkout of a
/// committed-source project should produce zero divergence.
pub async fn check_pipeline<H, P>(
    manifest: &Manifest,
    manifest_root: &Path,
    host: &H,
    provider: &P,
    inline_invocations: Vec<wado_compiler::kiln::Invocation>,
) -> Result<CheckOutcome, PipelineError>
where
    H: CompilerHost,
    P: GeneratorProvider,
{
    let plan_order =
        wado_compiler::kiln::build_plan(inline_invocations).map_err(DriverError::Plan)?;
    let mut planned = PlanOutcome {
        plan: plan_order,
        manifest_root: manifest_root.to_path_buf(),
    };
    if planned.plan.order.is_empty() {
        return Ok(CheckOutcome::default());
    }

    let resolved = resolve_modules(&planned.plan.order, provider, host).await;
    typed_encode_options(manifest, &mut planned.plan.order, &resolved, host);

    let mut outcome = CheckOutcome::default();
    for invocation in &planned.plan.order {
        let invocation_name = invocation_id(invocation);

        let generator = lookup_resolved(&resolved, &invocation.module).map_err(|source| {
            PipelineError::Provider {
                invocation: invocation_name.clone(),
                source,
            }
        })?;
        let run = execute_with_mode(
            invocation,
            &generator.wasm,
            manifest_root,
            host,
            ExecuteMode::DryRun,
        )
        .await
        .map_err(|source| PipelineError::Execute {
            invocation: invocation_name.clone(),
            source,
        })?;
        outcome.checked.push(invocation_name.clone());

        if let Some(entry_path) = run.outputs.iter().find(|o| o.is_entry).map(|o| &o.path) {
            let decl_file = invocation.decl_site.module.clone();
            let joined = manifest_root.join(entry_path);
            let abs = std::fs::canonicalize(&joined).unwrap_or(joined);
            let uri = path_to_kiln_uri(&abs);
            outcome
                .invocations
                .insert(&decl_file, invocation.from.as_str(), &uri);
        }

        for output in &run.outputs {
            let abs = manifest_root.join(&output.path);
            match std::fs::read(&abs) {
                Ok(existing) => {
                    if existing != output.bytes {
                        emit_stale_on_disk_warning(host, &invocation_name, &output.path);
                        outcome.stale.push(output.path.clone());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    emit_missing_on_disk_warning(host, &invocation_name, &output.path);
                    outcome.missing.push(output.path.clone());
                }
                Err(source) => {
                    return Err(PipelineError::Io { path: abs, source });
                }
            }
        }
    }
    Ok(outcome)
}

fn emit_stale_on_disk_warning<H: CompilerHost>(host: &H, invocation: &str, path: &str) {
    use wado_compiler::{Code, Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnGeneratedStaleOnDisk,
        message: format!(
            "kiln[{invocation}]: {path} differs from generator output; \
             commit the regenerated file or revert the local edit",
        ),
        span: None,
    });
}

fn emit_missing_on_disk_warning<H: CompilerHost>(host: &H, invocation: &str, path: &str) {
    use wado_compiler::{Code, Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnGeneratedStaleOnDisk,
        message: format!(
            "kiln[{invocation}]: {path} is missing on disk but the generator produced it; \
             run `wado compile` to materialize it",
        ),
        span: None,
    });
}

fn is_unsupported(err: &PipelineError) -> bool {
    matches!(
        err,
        PipelineError::Execute {
            source: ExecuteError::Runner(GeneratorRunnerError::Unsupported),
            ..
        } | PipelineError::Provider {
            source: ProviderError::Unsupported { .. },
            ..
        }
    )
}

fn emit_stale_warning<H: CompilerHost>(host: &H, invocation: &str) {
    use wado_compiler::{Code, Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::KilnStaleCache,
        message: format!(
            "kiln[{invocation}]: generator execution is not available in this host; \
             re-run `wado compile` natively to refresh `build/kiln/` and `wado.lock`",
        ),
        span: None,
    });
}

/// Resolve every unique [`GeneratorModule`] in `order` exactly once.
///
/// Resolution errors are folded into the returned map rather than
/// bubbled up: whether a per-module failure aborts the pipeline,
/// degrades to a stale-cache warning, or just skips option validation
/// is a per-invocation policy decision that lives in the run loop.
///
/// `Vec` not `HashMap`: the unique-module count is O(1) in practice
/// (typically a single generator per project) and `GeneratorModule`
/// isn't `Hash`.
async fn resolve_modules<H, P>(
    order: &[Invocation],
    provider: &P,
    host: &H,
) -> Vec<(
    GeneratorModule,
    Result<Arc<ResolvedGenerator>, ProviderError>,
)>
where
    H: CompilerHost,
    P: GeneratorProvider,
{
    let _s = KilnSpan::new(host, "kiln/resolve");
    let mut out: Vec<(
        GeneratorModule,
        Result<Arc<ResolvedGenerator>, ProviderError>,
    )> = Vec::new();
    for inv in order {
        if out.iter().any(|(m, _)| m == &inv.module) {
            continue;
        }
        let result = provider.resolve(&inv.module).await.map(Arc::new);
        out.push((inv.module.clone(), result));
    }
    out
}

/// Project the resolved-modules map into a per-invocation result. The
/// `Err` arm is cloned (rather than referenced) so each invocation
/// sharing a broken module can fold an owned error into its own
/// outcome independently.
fn lookup_resolved(
    resolved: &[(
        GeneratorModule,
        Result<Arc<ResolvedGenerator>, ProviderError>,
    )],
    module: &GeneratorModule,
) -> Result<Arc<ResolvedGenerator>, ProviderError> {
    resolved
        .iter()
        .find(|(m, _)| m == module)
        .map(|(_, r)| match r {
            Ok(arc) => Ok(Arc::clone(arc)),
            Err(e) => Err(e.clone()),
        })
        .expect("resolve_modules populates every module referenced by the plan")
}

/// Re-encode each invocation's `options_canonical` against the typed
/// descriptor when one is available, falling back silently to the
/// provisional bytes [`lower`] produced when no descriptor exists
/// (`descriptor: None`) or the module failed to resolve at all
/// (`Err` — the run loop will handle the failure as a per-invocation
/// stale-cache warning or pipeline error).
///
/// Validation failures (unknown / missing / type-mismatched fields)
/// surface as error diagnostics on `host`; the provisional bytes stay
/// in place so downstream phases still see a consistent invocation
/// and the generator-side trap surfaces in the same run as the
/// compiler-side complaint.
fn typed_encode_options<H: CompilerHost>(
    manifest: &Manifest,
    invocations: &mut [Invocation],
    resolved: &[(
        GeneratorModule,
        Result<Arc<ResolvedGenerator>, ProviderError>,
    )],
    host: &H,
) {
    let _ = manifest;
    for inv in invocations.iter_mut() {
        let descriptor = match lookup_resolved(resolved, &inv.module) {
            Ok(arc) => match &arc.descriptor {
                Some(d) => d.clone(),
                None => continue,
            },
            Err(_) => continue,
        };

        let supplied: Option<&AttrValue> = match inv.raw_options.as_ref() {
            Some(AttrValue::Object(obj)) if obj.is_empty() => None,
            other => other,
        };
        match validate_options(&descriptor, supplied) {
            Ok(canonical) => {
                inv.options_canonical = encode_options_canonical(&canonical);
            }
            Err(diagnostics) => {
                for d in diagnostics {
                    host.emit_diagnostic(d);
                }
            }
        }
    }
}

async fn run_and_build_metadata<H>(
    invocation_name: &str,
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
    generator: &ResolvedGenerator,
    options_hash: String,
) -> Result<Metadata, PipelineError>
where
    H: CompilerHost,
{
    let run = execute(invocation, &generator.wasm, manifest_root, host)
        .await
        .map_err(|source| PipelineError::Execute {
            invocation: invocation_name.to_string(),
            source,
        })?;
    Ok(build_metadata(
        invocation_name,
        invocation,
        &run,
        options_hash,
        generator.source_hash.clone(),
    ))
}

fn invocation_id(inv: &Invocation) -> String {
    inv.decl_site.synthetic_id.clone()
}

fn emit_metadata_load_warning<H: CompilerHost>(
    host: &H,
    invocation: &str,
    source: &kiln_metadata::MetadataError,
) {
    use wado_compiler::{Code, Diagnostic, Severity};
    host.emit_diagnostic(Diagnostic {
        severity: Severity::Warning,
        code: Code::Log,
        message: format!(
            "kiln[{invocation}]: failed to read cache file ({source}); \
             treating as cache miss and re-running generator",
        ),
        span: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    mod execute_tests {
        use super::*;
        use indexmap::IndexMap;
        use std::sync::Mutex;
        use wado_compiler::compiler_host::{
            Diagnostic, GeneratorOutputFile, GeneratorResponse, GeneratorRunnerError,
        };

        struct MockHost {
            sources: IndexMap<String, Vec<u8>>,
            response: Mutex<Option<Result<GeneratorResponse, GeneratorRunnerError>>>,
            requests: Mutex<Vec<GeneratorRequest>>,
        }

        impl MockHost {
            fn new(
                sources: &[(&str, &[u8])],
                response: Result<GeneratorResponse, GeneratorRunnerError>,
            ) -> Self {
                let sources = sources
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                    .collect();
                Self {
                    sources,
                    response: Mutex::new(Some(response)),
                    requests: Mutex::new(Vec::new()),
                }
            }
        }

        impl CompilerHost for MockHost {
            async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
                self.sources
                    .get(path)
                    .cloned()
                    .ok_or_else(|| SourceError::NotFound {
                        path: path.to_string(),
                    })
            }

            fn emit_diagnostic(&self, _d: Diagnostic) {}

            async fn run_generator(
                &self,
                _wasm: &[u8],
                request: GeneratorRequest,
            ) -> Result<GeneratorResponse, GeneratorRunnerError> {
                self.requests.lock().unwrap().push(request);
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("mock response already consumed")
            }
        }

        fn sample_invocation() -> Invocation {
            Invocation {
                decl_site: DeclSite {
                    module: "src/main.wado".to_string(),
                    synthetic_id: "kiln-proto".to_string(),
                },
                module: GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
                from: InvocationPath::normalize("schema.proto"),
                inputs: vec![InvocationPath::normalize("dep.proto")],
                output_dir: InvocationPath::normalize("build/kiln/proto"),
                options_canonical: vec![],
                raw_options: None,
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        #[test]
        fn writes_outputs_with_header_and_reports_hashes() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![
                    GeneratorOutputFile {
                        path: "lib.wado".to_string(),
                        content: "pub fn hello() {}\n".to_string(),
                        is_entry: true,
                    },
                    GeneratorOutputFile {
                        path: "sub/mod.wado".to_string(),
                        content: "pub fn sub() {}\n".to_string(),
                        is_entry: false,
                    },
                ],
                reads: vec![],
            };
            let host = MockHost::new(
                &[
                    ("schema.proto", b"syntax = \"proto3\";"),
                    ("dep.proto", b"import \"other.proto\";"),
                ],
                Ok(response),
            );

            let inv = sample_invocation();
            let run = runtime()
                .block_on(async { execute(&inv, b"wasm-bytes", tmp.path(), &host).await })
                .unwrap();

            assert_eq!(run.outputs.len(), 2);
            assert_eq!(run.outputs[0].path, "build/kiln/proto/lib.wado");
            assert!(run.outputs[0].is_entry);
            assert_eq!(run.outputs[1].path, "build/kiln/proto/sub/mod.wado");
            assert!(!run.outputs[1].is_entry);

            let lib =
                std::fs::read_to_string(tmp.path().join("build/kiln/proto/lib.wado")).unwrap();
            assert!(lib.starts_with("#![generated(by = \"ns:proto@1.0.0\""));
            assert!(lib.contains("\"schema.proto\""));
            assert!(lib.contains("\"dep.proto\""));
            assert!(lib.ends_with("pub fn hello() {}\n"));

            let nested =
                std::fs::read_to_string(tmp.path().join("build/kiln/proto/sub/mod.wado")).unwrap();
            assert!(nested.starts_with("#![generated("));
        }

        #[test]
        fn execute_surfaces_runner_unsupported_verbatim() {
            let tmp = tempfile::tempdir().unwrap();
            let host = MockHost::new(
                &[("schema.proto", b"x"), ("dep.proto", b"y")],
                Err(GeneratorRunnerError::Unsupported),
            );
            let err = runtime()
                .block_on(async { execute(&sample_invocation(), b"wasm", tmp.path(), &host).await })
                .unwrap_err();
            assert!(matches!(
                err,
                ExecuteError::Runner(GeneratorRunnerError::Unsupported)
            ));
        }

        #[test]
        fn missing_primary_surfaces_load_input_error() {
            let tmp = tempfile::tempdir().unwrap();
            let host = MockHost::new(
                &[("dep.proto", b"y")],
                Err(GeneratorRunnerError::Unsupported),
            );
            let err = runtime()
                .block_on(async { execute(&sample_invocation(), b"wasm", tmp.path(), &host).await })
                .unwrap_err();
            match err {
                ExecuteError::LoadInput { path, .. } => assert_eq!(path, "schema.proto"),
                other => panic!("expected LoadInput, got {other:?}"),
            }
        }

        #[test]
        fn escaping_output_path_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "../escape.wado".to_string(),
                    content: "x".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = MockHost::new(&[("schema.proto", b"x"), ("dep.proto", b"y")], Ok(response));
            let err = runtime()
                .block_on(async { execute(&sample_invocation(), b"wasm", tmp.path(), &host).await })
                .unwrap_err();
            match err {
                ExecuteError::InvalidOutputPath { path } => assert_eq!(path, "../escape.wado"),
                other => panic!("expected InvalidOutputPath, got {other:?}"),
            }
        }

        #[test]
        fn options_canonical_bytes_are_forwarded_to_request() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![],
            };
            let host = MockHost::new(&[("schema.proto", b"x"), ("dep.proto", b"y")], Ok(response));
            let mut inv = sample_invocation();
            // Canonical options travel to the generator as a UTF-8 JSON
            // string (see WEP §"The `kiln` world"). Use a well-formed
            // canonical JSON fixture so the UTF-8 invariant holds.
            inv.options_canonical = br#"{"k":"v"}"#.to_vec();

            runtime()
                .block_on(async { execute(&inv, b"wasm", tmp.path(), &host).await })
                .unwrap();
            let reqs = host.requests.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].options, r#"{"k":"v"}"#);
            assert_eq!(reqs[0].primary.path, "schema.proto");
            assert_eq!(reqs[0].inputs.len(), 1);
            assert_eq!(reqs[0].inputs[0].path, "dep.proto");
        }

        #[test]
        fn reads_from_response_are_relayed_into_run() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![GeneratorReadRecord {
                    path: "extra.proto".to_string(),
                    content_hash: [7u8; 32],
                }],
            };
            let host = MockHost::new(&[("schema.proto", b"x"), ("dep.proto", b"y")], Ok(response));
            let run = runtime()
                .block_on(async { execute(&sample_invocation(), b"wasm", tmp.path(), &host).await })
                .unwrap();
            assert_eq!(run.reads.len(), 1);
            assert_eq!(run.reads[0].path, "extra.proto");
        }
    }

    mod cache_tests {
        use super::*;
        use indexmap::IndexMap;
        use std::sync::Mutex;
        use wado_compiler::compiler_host::{
            Diagnostic, GeneratorOutputFile, GeneratorResponse, GeneratorRunnerError,
        };

        struct HashOnlyHost {
            sources: IndexMap<String, Vec<u8>>,
        }

        impl HashOnlyHost {
            fn new(sources: &[(&str, &[u8])]) -> Self {
                Self {
                    sources: sources
                        .iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                        .collect(),
                }
            }
        }

        impl CompilerHost for HashOnlyHost {
            async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
                self.sources
                    .get(path)
                    .cloned()
                    .ok_or_else(|| SourceError::NotFound {
                        path: path.to_string(),
                    })
            }

            fn emit_diagnostic(&self, _d: Diagnostic) {}
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        fn sample_invocation() -> Invocation {
            Invocation {
                decl_site: DeclSite {
                    module: "src/main.wado".to_string(),
                    synthetic_id: "kiln-proto".to_string(),
                },
                module: GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
                from: InvocationPath::normalize("schema.proto"),
                inputs: vec![InvocationPath::normalize("dep.proto")],
                output_dir: InvocationPath::normalize("build/kiln/proto"),
                options_canonical: vec![],
                raw_options: None,
            }
        }

        fn run_execute_and_return(
            tmp: &std::path::Path,
            response: GeneratorResponse,
            sources: &[(&str, &[u8])],
        ) -> InvocationRun {
            struct H {
                sources: IndexMap<String, Vec<u8>>,
                response: Mutex<Option<Result<GeneratorResponse, GeneratorRunnerError>>>,
            }
            impl CompilerHost for H {
                async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
                    self.sources
                        .get(path)
                        .cloned()
                        .ok_or_else(|| SourceError::NotFound {
                            path: path.to_string(),
                        })
                }
                fn emit_diagnostic(&self, _d: Diagnostic) {}
                async fn run_generator(
                    &self,
                    _wasm: &[u8],
                    _request: GeneratorRequest,
                ) -> Result<GeneratorResponse, GeneratorRunnerError> {
                    self.response
                        .lock()
                        .unwrap()
                        .take()
                        .expect("already consumed")
                }
            }
            let host = H {
                sources: sources
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                    .collect(),
                response: Mutex::new(Some(Ok(response))),
            };
            runtime()
                .block_on(async { execute(&sample_invocation(), b"wasm", tmp, &host).await })
                .unwrap()
        }

        #[test]
        fn build_metadata_round_trips_paths_and_hashes() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "lib.wado".to_string(),
                    content: "pub fn hello() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![GeneratorReadRecord {
                    path: "imported.proto".to_string(),
                    content_hash: [7u8; 32],
                }],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"primary"), ("dep.proto", b"dep")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:optdigest".to_string(),
                String::new(),
            );

            assert_eq!(entry.invocation, "proto");
            assert_eq!(entry.generator, "ns:proto@1.0.0");
            assert_eq!(entry.primary.path, "schema.proto");
            assert_eq!(entry.primary.hash.len(), 64);
            assert!(entry.primary.hash.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(entry.inputs.len(), 1);
            assert_eq!(entry.inputs[0].path, "dep.proto");
            assert_eq!(entry.reads.len(), 1);
            assert_eq!(entry.reads[0].path, "imported.proto");
            assert_eq!(entry.options_hash, "sha256:optdigest");
            assert_eq!(entry.outputs.len(), 1);
            assert_eq!(entry.outputs[0].path, "build/kiln/proto/lib.wado");
            assert!(entry.outputs[0].entry);
        }

        #[test]
        fn build_metadata_sorts_reads_lexicographically() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![
                    GeneratorReadRecord {
                        path: "z.proto".to_string(),
                        content_hash: [1u8; 32],
                    },
                    GeneratorReadRecord {
                        path: "a.proto".to_string(),
                        content_hash: [2u8; 32],
                    },
                    GeneratorReadRecord {
                        path: "a.proto".to_string(),
                        content_hash: [2u8; 32],
                    },
                ],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            assert_eq!(entry.reads.len(), 2);
            assert_eq!(entry.reads[0].path, "a.proto");
            assert_eq!(entry.reads[1].path, "z.proto");
        }

        #[test]
        fn cache_matches_hits_when_everything_lines_up() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "lib.wado".to_string(),
                    content: "pub fn hello() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            let host = HashOnlyHost::new(&[("schema.proto", b"p"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host, "").await
            });
            assert_eq!(hit, CacheCheck::Hit);
        }

        #[test]
        fn cache_matches_misses_when_primary_changed() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            let host = HashOnlyHost::new(&[("schema.proto", b"different"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host, "").await
            });
            assert_eq!(hit, CacheCheck::Miss);
        }

        #[test]
        fn cache_matches_misses_when_output_deleted() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "lib.wado".to_string(),
                    content: "pub fn hello() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            std::fs::remove_file(tmp.path().join("build/kiln/proto/lib.wado")).unwrap();
            let host = HashOnlyHost::new(&[("schema.proto", b"p"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host, "").await
            });
            assert_eq!(hit, CacheCheck::Miss);
        }

        #[test]
        fn cache_matches_returns_hit_but_modified_when_output_edited_in_place() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "lib.wado".to_string(),
                    content: "pub fn hello() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            // Hand-edit the generated file after generation.
            std::fs::write(
                tmp.path().join("build/kiln/proto/lib.wado"),
                b"// edited by hand\n",
            )
            .unwrap();
            let host = HashOnlyHost::new(&[("schema.proto", b"p"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host, "").await
            });
            assert_eq!(hit, CacheCheck::HitButModified);
        }

        #[test]
        fn cache_matches_misses_when_reads_drift() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![GeneratorReadRecord {
                    path: "imported.proto".to_string(),
                    content_hash: wado_compiler::kiln::file_hash(
                        &InvocationPath::normalize("imported.proto"),
                        b"original",
                    )
                    .hash,
                }],
            };
            let run = run_execute_and_return(
                tmp.path(),
                response,
                &[("schema.proto", b"p"), ("dep.proto", b"d")],
            );
            let entry = build_metadata(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:o".to_string(),
                String::new(),
            );
            let host = HashOnlyHost::new(&[
                ("schema.proto", b"p"),
                ("dep.proto", b"d"),
                ("imported.proto", b"mutated"),
            ]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host, "").await
            });
            assert_eq!(hit, CacheCheck::Miss);
        }

        #[test]
        fn reconcile_outputs_deletes_orphaned_generated_files() {
            let tmp = tempfile::tempdir().unwrap();
            let out_dir = tmp.path().join("build/kiln/proto");
            std::fs::create_dir_all(&out_dir).unwrap();

            let kept_path = out_dir.join("kept.wado");
            let orphan_path = out_dir.join("orphan.wado");
            let hand_written_path = out_dir.join("hand.wado");
            std::fs::write(
                &kept_path,
                "#![generated(by = \"gen\", sources = [])]\npub fn k() {}\n",
            )
            .unwrap();
            std::fs::write(
                &orphan_path,
                "#![generated(by = \"gen\", sources = [])]\npub fn o() {}\n",
            )
            .unwrap();
            std::fs::write(&hand_written_path, "pub fn h() {}\n").unwrap();

            let deleted = reconcile_outputs(
                tmp.path(),
                &out_dir,
                &["build/kiln/proto/kept.wado".to_string()],
            )
            .unwrap();

            assert_eq!(deleted, vec!["build/kiln/proto/orphan.wado".to_string()]);
            assert!(kept_path.exists());
            assert!(!orphan_path.exists());
            assert!(hand_written_path.exists());
        }

        #[test]
        fn reconcile_outputs_missing_dir_is_empty() {
            let tmp = tempfile::tempdir().unwrap();
            let deleted =
                reconcile_outputs(tmp.path(), &tmp.path().join("no-such-dir"), &[]).unwrap();
            assert!(deleted.is_empty());
        }

        #[test]
        fn reconcile_outputs_descends_into_subdirs() {
            let tmp = tempfile::tempdir().unwrap();
            let nested = tmp.path().join("build/kiln/proto/sub");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(
                nested.join("orphan.wado"),
                "#![generated(by = \"gen\", sources = [])]\n",
            )
            .unwrap();
            let deleted =
                reconcile_outputs(tmp.path(), &tmp.path().join("build/kiln/proto"), &[]).unwrap();
            assert_eq!(
                deleted,
                vec!["build/kiln/proto/sub/orphan.wado".to_string()]
            );
        }
    }
}
