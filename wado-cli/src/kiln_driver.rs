//! Kiln pipeline driver — manifest-lowering, plan computation, generator
//! execution, and cache bookkeeping.
//!
//! This is the CLI-side glue between [`wado_manifest::Manifest`] and the
//! pure-data algorithmic module at [`wado_compiler::kiln`]. Its
//! responsibilities are:
//!
//! 1. Lower manifest invocations to the compiler's canonical form and
//!    dedup + topologically sort them via [`plan`].
//! 2. Execute a single invocation by calling
//!    [`wado_compiler::compiler_host::CompilerHost::run_generator`] and
//!    writing the response to disk under the invocation's output
//!    directory, with a canonical `#![generated]` header stamped on each
//!    file — see [`execute`].
//! 3. Convert an [`InvocationRun`] into a [`wado_manifest::GeneratorCacheEntry`]
//!    for persistence (`build_cache_entry`), check a recorded entry for
//!    freshness against the current filesystem + sources (`cache_matches`),
//!    and delete orphaned `#![generated]` files left over from an earlier
//!    run (`reconcile_outputs`).
//!
//! Wiring into the top-level `wado compile` flow lands in a subsequent
//! commit; this module stays agnostic of where the driver is called from.

use std::path::{Path, PathBuf};

use wado_compiler::ast::AttrValue;
use wado_compiler::compiler_host::{
    CompilerHost, GeneratorInputFile, GeneratorReadRecord, GeneratorRequest, GeneratorRunnerError,
    SourceError,
};
use wado_compiler::hashmap::IndexMap as CompilerIndexMap;
use wado_compiler::kiln::{
    DeclSite, FileHash, GeneratedHeader, GeneratorModule, Invocation, InvocationPath,
    OptionsDescriptor, Plan, PlanError, build_plan, content_hash, encode_options_canonical,
    file_hash, generator_identity, has_generated_marker, hex_digest, validate_options,
};
use wado_manifest::{
    FileHash as ManifestFileHash, GeneratorCacheEntry, GeneratorInvocation, GeneratorModuleRef,
    Manifest, OutputHash as ManifestOutputHash,
};

/// Output directory default when `[build.generators.<name>].output_dir` is
/// unset: `build/kiln/<name>`.
pub const DEFAULT_OUTPUT_DIR_PREFIX: &str = "build/kiln";

/// Outcome of [`plan`].
#[derive(Debug)]
pub struct PlanOutcome {
    /// Scheduled invocations in execution order. Empty when the manifest has
    /// no `[build.generators]` section.
    pub plan: Plan,
    /// The manifest root used to resolve relative paths, preserved so the
    /// caller can read input files without re-deriving it.
    pub manifest_root: PathBuf,
}

/// Errors from the driver. Reuses the compiler-side [`PlanError`] for
/// dedup/cycle diagnostics and adds CLI-specific variants for the
/// lowering step.
#[derive(Debug)]
pub enum DriverError {
    /// The manifest's `module = { ... }` inline form is not yet supported by
    /// the driver. M3c ships with `module = "ns:name@ver"` only; inline
    /// records land together with build-dependency resolution.
    UnsupportedInlineModule { invocation: String },
    /// Dedup / cycle error from the planner.
    Plan(PlanError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::UnsupportedInlineModule { invocation } => write!(
                f,
                "[build.generators.{invocation}] uses an inline `module = {{ ... }}` record; \
                 only `module = \"ns:name@version\"` is supported in M3c"
            ),
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

/// Compute a plan for the manifest's `[build.generators]` section.
///
/// Takes an already-parsed manifest and the directory it was loaded from
/// (the `manifest_root`). Paths in `from` / `inputs` / `output_dir` are
/// interpreted relative to that root; the produced [`Invocation`]s carry
/// root-relative normalized paths.
///
/// Options are opaque to the driver at this stage: the canonical encoding
/// is produced by [`encode_options_canonical_provisional`] and passed
/// through as `options_canonical` bytes. M4 swaps the encoder without
/// changing this driver.
///
/// # Errors
///
/// - [`DriverError::UnsupportedInlineModule`] if any invocation uses inline
///   `module = { ... }`.
/// - [`DriverError::Plan`] for cycle / duplicate-generator errors.
///
/// The module-spec ↔ `[build-dependencies]` match is guaranteed by
/// `wado-manifest`'s `FromStr` validation, so this function trusts the
/// caller to pass a validated [`Manifest`].
pub fn plan(manifest: &Manifest, manifest_root: &Path) -> Result<PlanOutcome, DriverError> {
    let Some(build) = manifest.build.as_ref() else {
        return Ok(PlanOutcome {
            plan: Plan { order: Vec::new() },
            manifest_root: manifest_root.to_path_buf(),
        });
    };

    let mut invocations: Vec<Invocation> = Vec::with_capacity(build.generators.len());
    for (name, gi) in &build.generators {
        invocations.push(lower(name, gi)?);
    }

    let plan = build_plan(invocations)?;
    Ok(PlanOutcome {
        plan,
        manifest_root: manifest_root.to_path_buf(),
    })
}

/// Lower a single manifest-declared invocation to the compiler's canonical
/// form.
fn lower(name: &str, gi: &GeneratorInvocation) -> Result<Invocation, DriverError> {
    let module = match &gi.module {
        GeneratorModuleRef::Spec(spec) => GeneratorModule::Spec(spec.clone()),
        GeneratorModuleRef::Inline(_) => {
            return Err(DriverError::UnsupportedInlineModule {
                invocation: name.to_string(),
            });
        }
    };

    let from = InvocationPath::normalize(&gi.from);
    let inputs: Vec<InvocationPath> = gi
        .inputs
        .iter()
        .map(|p| InvocationPath::normalize(p))
        .collect();
    let output_dir = InvocationPath::normalize(
        gi.output_dir
            .as_deref()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| format!("{DEFAULT_OUTPUT_DIR_PREFIX}/{name}"))
            .as_str(),
    );
    let options_canonical = encode_options_canonical_provisional(&gi.options);
    Ok(Invocation {
        decl_site: DeclSite::Manifest {
            name: name.to_string(),
        },
        module,
        from,
        inputs,
        output_dir,
        options_canonical,
    })
}

/// Provisional canonical TOML encoder — used by M3 until M4 replaces it with
/// the Component-Model lifted form.
///
/// Format (length-prefixed, depth-first, keys sorted alphabetically):
/// - `0` bool, 1 byte (0 / 1)
/// - `1` integer, 8 BE bytes
/// - `2` float, 8 BE bytes (IEEE-754 bits)
/// - `3` string, u64 BE length-prefix + UTF-8 bytes
/// - `4` array, u64 BE count + each element
/// - `5` table, u64 BE count + each (key string + value)
/// - `6` datetime, u64 BE length-prefix + RFC3339 string
#[must_use]
pub fn encode_options_canonical_provisional(value: &toml::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_toml(&mut out, value);
    out
}

fn write_toml(out: &mut Vec<u8>, v: &toml::Value) {
    match v {
        toml::Value::Boolean(b) => {
            out.push(0);
            out.push(u8::from(*b));
        }
        toml::Value::Integer(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_be_bytes());
        }
        toml::Value::Float(f) => {
            out.push(2);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        toml::Value::String(s) => {
            out.push(3);
            write_str(out, s);
        }
        toml::Value::Array(a) => {
            out.push(4);
            write_len(out, a.len());
            for item in a {
                write_toml(out, item);
            }
        }
        toml::Value::Table(t) => {
            out.push(5);
            let mut entries: Vec<(&String, &toml::Value)> = t.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            write_len(out, entries.len());
            for (k, val) in entries {
                write_str(out, k);
                write_toml(out, val);
            }
        }
        toml::Value::Datetime(dt) => {
            out.push(6);
            write_str(out, &dt.to_string());
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn write_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_be_bytes());
}

/// Convert a `toml::Value` to the compiler-side generic [`AttrValue`] tree so
/// the driver can hand a user-supplied options table to
/// [`wado_compiler::kiln::validate_options`] without leaking `toml` into the
/// compiler crate.
///
/// Datetime values are rendered as their RFC-3339 string form (matching the
/// provisional encoder's tag 6). Floats are preserved; integers stay
/// `i64`-typed. Tables preserve declaration order via
/// [`wado_compiler::hashmap::IndexMap`] so downstream validation is
/// deterministic.
#[must_use]
pub fn toml_to_attr_value(v: &toml::Value) -> AttrValue {
    match v {
        toml::Value::Boolean(b) => AttrValue::Bool(*b),
        toml::Value::Integer(i) => AttrValue::Int(*i),
        toml::Value::Float(f) => AttrValue::Float(*f),
        toml::Value::String(s) => AttrValue::String(s.clone()),
        toml::Value::Datetime(dt) => AttrValue::String(dt.to_string()),
        toml::Value::Array(a) => AttrValue::Array(a.iter().map(toml_to_attr_value).collect()),
        toml::Value::Table(t) => {
            let mut entries: CompilerIndexMap<String, AttrValue> = CompilerIndexMap::default();
            for (k, val) in t {
                entries.insert(k.clone(), toml_to_attr_value(val));
            }
            AttrValue::Object(entries)
        }
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

/// Output-file identity written to `wado.lock`.
///
/// Produced by [`execute`], consumed by the cache-check / lockfile layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputHash {
    /// Project-root-relative forward-slash path of the written file.
    pub path: String,
    /// SHA-256 of the file contents as written (header + generator body).
    pub hash: [u8; 32],
    /// Whether the generator marked this file as the invocation's entry
    /// module — the one a consuming `use ... from "<from>"` resolves to.
    pub is_entry: bool,
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
    let primary = load_input(host, &invocation.from).await?;
    let primary_hash = file_hash(&invocation.from, &primary.content);
    let mut inputs = Vec::with_capacity(invocation.inputs.len());
    let mut input_hashes = Vec::with_capacity(invocation.inputs.len());
    for p in &invocation.inputs {
        let file = load_input(host, p).await?;
        input_hashes.push(file_hash(p, &file.content));
        inputs.push(file);
    }

    let request = GeneratorRequest {
        primary,
        inputs,
        options: invocation.options_canonical.clone(),
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
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ExecuteError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut bytes = Vec::with_capacity(header.len() + file.content.len());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(file.content.as_bytes());
        std::fs::write(&full_path, &bytes).map_err(|source| ExecuteError::Io {
            path: full_path.clone(),
            source,
        })?;

        let normalized = InvocationPath::normalize(&format!(
            "{}/{}",
            invocation.output_dir.as_str(),
            rel.to_string_lossy()
        ));
        outputs.push(OutputHash {
            path: normalized.as_str().to_string(),
            hash: file_hash(&normalized, &bytes).hash,
            is_entry: file.is_entry,
        });
    }

    Ok(InvocationRun {
        primary: primary_hash,
        inputs: input_hashes,
        outputs,
        reads: response.reads,
    })
}

/// Convert an [`InvocationRun`] into a [`GeneratorCacheEntry`] ready to be
/// written to `wado.lock`.
///
/// `invocation_name` is the lockfile-facing key (the `[build.generators.<k>]`
/// name for manifest invocations; `kiln-<hex>` for inline clauses landing in
/// M5). `options_hash` is the hex SHA-256 of the canonical options encoding —
/// produced by [`wado_compiler::kiln::hash_options_canonical`] so it stays
/// stable across the M3 provisional encoder and the M4 lifted-form encoder.
///
/// The emitted entry contains hex-encoded hashes (as required by
/// `wado-manifest`'s TOML form) and sorts the `reads` list lexicographically
/// by path, mirroring what [`cache_matches`] expects on the next run.
#[must_use]
pub fn build_cache_entry(
    invocation_name: &str,
    invocation: &Invocation,
    run: &InvocationRun,
    options_hash: String,
) -> GeneratorCacheEntry {
    let generator = generator_identity(&invocation.module);
    let primary = to_manifest_file_hash(&run.primary);
    let inputs: Vec<ManifestFileHash> = run.inputs.iter().map(to_manifest_file_hash).collect();

    let mut reads: Vec<ManifestFileHash> = run
        .reads
        .iter()
        .map(|r| ManifestFileHash {
            path: InvocationPath::normalize(&r.path).as_str().to_string(),
            hash: hex_digest(&r.content_hash),
        })
        .collect();
    reads.sort_by(|a, b| a.path.cmp(&b.path));
    reads.dedup_by(|a, b| a.path == b.path);

    let outputs: Vec<ManifestOutputHash> = run
        .outputs
        .iter()
        .map(|o| ManifestOutputHash {
            path: o.path.clone(),
            hash: hex_digest(&o.hash),
            entry: o.is_entry,
        })
        .collect();

    GeneratorCacheEntry {
        invocation: invocation_name.to_string(),
        generator,
        primary,
        inputs,
        options_hash,
        reads,
        outputs,
    }
}

fn to_manifest_file_hash(f: &FileHash) -> ManifestFileHash {
    ManifestFileHash {
        path: f.path.clone(),
        hash: hex_digest(&f.hash),
    }
}

/// Check whether a recorded cache entry is still valid for the given
/// invocation.
///
/// Re-hashes the primary + declared inputs via `host.load_source`, then
/// re-hashes every `reads` entry the same way, then re-hashes every output
/// file from disk (via `manifest_root` joined with the recorded output
/// path). Returns `true` iff every hash matches the lockfile record.
///
/// Any load or read failure — missing file, permissions, content drift —
/// is treated as a cache miss and surfaces as `Ok(false)`. Callers that
/// need to distinguish "stale cache" from "I/O broken" should fall through
/// to [`execute`]; it will surface the underlying error if the file is
/// genuinely missing at run time.
pub async fn cache_matches<H: CompilerHost>(
    entry: &GeneratorCacheEntry,
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
) -> bool {
    if entry.primary.path != invocation.from.as_str() {
        return false;
    }
    if !matches_file(host, &invocation.from, &entry.primary.hash).await {
        return false;
    }

    if entry.inputs.len() != invocation.inputs.len() {
        return false;
    }
    for (declared, recorded) in invocation.inputs.iter().zip(&entry.inputs) {
        if declared.as_str() != recorded.path {
            return false;
        }
        if !matches_file(host, declared, &recorded.hash).await {
            return false;
        }
    }

    for read in &entry.reads {
        let normalized = InvocationPath::normalize(&read.path);
        if !matches_file(host, &normalized, &read.hash).await {
            return false;
        }
    }

    for output in &entry.outputs {
        let abs = manifest_root.join(&output.path);
        match std::fs::read(&abs) {
            Ok(bytes) => {
                if !hash_matches_bytes(&bytes, &output.hash) {
                    return false;
                }
            }
            Err(source) => {
                emit_cache_io_warning(host, &abs, &source);
                return false;
            }
        }
    }
    true
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
    let content =
        host.load_source(path.as_str())
            .await
            .map_err(|source| ExecuteError::LoadInput {
                path: path.as_str().to_string(),
                source,
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

/// Resolves a generator's component bytes for execution.
///
/// The runner is deliberately decoupled from how a given module becomes a
/// component `.wasm`. Two concrete providers land in later commits:
///
/// - A production `TargetDirProvider` that compiles and caches generator
///   components under `target/kiln/generators/…` (ships with the M3d /
///   M6 fixtures, when there is a real generator to compile).
/// - A test provider that returns pre-built bytes.
pub trait GeneratorProvider {
    /// Resolve `module` to component bytes. Called at most once per unique
    /// module in a pipeline run.
    fn get_component(
        &self,
        module: &GeneratorModule,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ProviderError>> + Send;

    /// Resolve `module` to its typed [`OptionsDescriptor`]. Used by the
    /// driver to validate a user-supplied options table before calling the
    /// generator — see
    /// [`wado_compiler::kiln::validate_options`]. When a provider cannot
    /// introspect the generator (e.g. consume-only mode on the LSP), it
    /// returns [`ProviderError::Unsupported`] and the driver falls back to
    /// the provisional TOML encoder on the raw options table.
    fn descriptor(
        &self,
        module: &GeneratorModule,
    ) -> impl std::future::Future<Output = Result<OptionsDescriptor, ProviderError>> + Send {
        let _ = module;
        async {
            Err(ProviderError::Unsupported {
                message:
                    "kiln: provider does not expose OptionsDescriptor (typed options unavailable)"
                        .to_string(),
            })
        }
    }
}

/// Error returned by a [`GeneratorProvider`].
#[derive(Debug)]
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
    /// Reading or writing `wado.lock` or an output directory failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Parsing an existing `wado.lock` failed.
    LockParse {
        source: wado_manifest::LockFileError,
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
            PipelineError::LockParse { source } => write!(f, "kiln: invalid wado.lock: {source}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<DriverError> for PipelineError {
    fn from(e: DriverError) -> Self {
        PipelineError::Driver(e)
    }
}

/// Run the full Kiln pipeline for a manifest: plan → per-invocation cache
/// check → execute on miss → reconcile stale outputs → persist lockfile.
///
/// `provider` resolves generator module bytes on demand. `host` is the
/// compiler host used to load input files and (inside `execute`) to invoke
/// the runner.
///
/// On success, the manifest's `wado.lock` is updated in place to persist
/// the current `[[generator-cache]]` entries. The lockfile is re-serialized
/// via [`wado_manifest::LockFile::to_toml`], so non-Kiln sections round-trip
/// through the manifest-crate writer — byte-identity is not guaranteed.
///
/// Returns an empty outcome if the manifest has no `[build.generators]`.
///
/// # Errors
/// See [`PipelineError`].
pub async fn run_pipeline<H, P>(
    manifest: &Manifest,
    manifest_root: &Path,
    host: &H,
    provider: &P,
) -> Result<PipelineOutcome, PipelineError>
where
    H: CompilerHost,
    P: GeneratorProvider,
{
    let mut planned = plan(manifest, manifest_root)?;
    if planned.plan.order.is_empty() {
        return Ok(PipelineOutcome::default());
    }

    typed_encode_options(manifest, &mut planned.plan.order, provider, host).await;

    let mut lock = load_lockfile(manifest_root)?;
    let mut new_cache: Vec<GeneratorCacheEntry> = Vec::with_capacity(planned.plan.order.len());
    let mut outcome = PipelineOutcome::default();
    let mut kept_by_dir: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();

    for invocation in &planned.plan.order {
        let invocation_name = invocation_id(invocation);

        let existing = lock
            .generator_cache
            .iter()
            .find(|e| e.invocation == invocation_name)
            .cloned();

        let options_hash =
            wado_compiler::kiln::hash_options_canonical(&invocation.options_canonical);

        let (entry, executed) =
            if let Some(prior) = existing.clone().filter(|e| e.options_hash == options_hash) {
                if cache_matches(&prior, invocation, manifest_root, host).await {
                    outcome.cached.push(invocation_name.clone());
                    (Some(prior), false)
                } else {
                    match run_and_build_entry(
                        &invocation_name,
                        invocation,
                        manifest_root,
                        host,
                        provider,
                        options_hash.clone(),
                    )
                    .await
                    {
                        Ok(entry) => {
                            outcome.executed.push(invocation_name.clone());
                            (Some(entry), true)
                        }
                        Err(e) if is_unsupported(&e) => {
                            emit_stale_warning(host, &invocation_name);
                            outcome.stale.push(invocation_name.clone());
                            (Some(prior), false)
                        }
                        Err(e) => return Err(e),
                    }
                }
            } else {
                match run_and_build_entry(
                    &invocation_name,
                    invocation,
                    manifest_root,
                    host,
                    provider,
                    options_hash,
                )
                .await
                {
                    Ok(entry) => {
                        outcome.executed.push(invocation_name.clone());
                        (Some(entry), true)
                    }
                    Err(e) if is_unsupported(&e) => {
                        emit_stale_warning(host, &invocation_name);
                        outcome.stale.push(invocation_name.clone());
                        (existing, false)
                    }
                    Err(e) => return Err(e),
                }
            };

        if let Some(entry) = entry {
            if executed {
                let dir = invocation.output_dir.as_str().to_string();
                let kept = kept_by_dir.entry(dir).or_default();
                for o in &entry.outputs {
                    kept.push(o.path.clone());
                }
            }
            new_cache.push(entry);
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

    lock.generator_cache = new_cache;

    save_lockfile(manifest_root, &lock)?;
    Ok(outcome)
}

fn is_unsupported(err: &PipelineError) -> bool {
    match err {
        PipelineError::Execute {
            source: ExecuteError::Runner(GeneratorRunnerError::Unsupported),
            ..
        }
        | PipelineError::Provider {
            source: ProviderError::Unsupported { .. },
            ..
        } => true,
        _ => false,
    }
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

/// Re-encode each invocation's `options_canonical` bytes using the typed
/// pipeline from [`wado_compiler::kiln::encode_options_canonical`] when the
/// provider exposes a descriptor. Falls back silently to the provisional
/// bytes already produced by [`lower`] when:
///
/// - the invocation is anonymous (inline `use ... with`) — M5 clauses already
///   encode via the typed path when they are built, so this code path skips
///   them;
/// - the provider returns [`ProviderError::Unsupported`] (e.g. consume-only
///   LSP mode);
/// - the provider returns [`ProviderError::Internal`] — a warning diagnostic
///   is surfaced through `host.emit_diagnostic`, but the pipeline continues.
///
/// Validation failures (unknown / missing / type-mismatched fields) surface
/// as error diagnostics on `host`; the provisional bytes remain in place so
/// downstream layers still see a consistent invocation. The caller's next
/// step — `run_and_build_entry` — will fail fast when the generator rejects
/// the options blob, so the user sees both the compiler-side validation
/// error and the generator-side trap in the same run.
async fn typed_encode_options<H, P>(
    manifest: &Manifest,
    invocations: &mut [Invocation],
    provider: &P,
    host: &H,
) where
    H: CompilerHost,
    P: GeneratorProvider,
{
    use wado_compiler::{Code, Diagnostic, Severity};

    let Some(build) = manifest.build.as_ref() else {
        return;
    };

    for inv in invocations.iter_mut() {
        let name = match &inv.decl_site {
            DeclSite::Manifest { name } => name.clone(),
            DeclSite::Inline { .. } => continue,
        };
        let Some(gi) = build.generators.get(&name) else {
            continue;
        };

        let descriptor = match provider.descriptor(&inv.module).await {
            Ok(d) => d,
            Err(ProviderError::Unsupported { .. }) => continue,
            Err(ProviderError::Internal { message }) => {
                host.emit_diagnostic(Diagnostic {
                    severity: Severity::Warning,
                    code: Code::Log,
                    message: format!(
                        "kiln[{name}]: failed to introspect generator options schema ({message}); \
                         falling back to raw TOML encoding",
                    ),
                    span: None,
                });
                continue;
            }
        };

        let attr = toml_to_attr_value(&gi.options);
        let supplied: Option<&AttrValue> = match &attr {
            AttrValue::Object(obj) if obj.is_empty() => None,
            other => Some(other),
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

async fn run_and_build_entry<H, P>(
    invocation_name: &str,
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
    provider: &P,
    options_hash: String,
) -> Result<GeneratorCacheEntry, PipelineError>
where
    H: CompilerHost,
    P: GeneratorProvider,
{
    let component = provider
        .get_component(&invocation.module)
        .await
        .map_err(|source| PipelineError::Provider {
            invocation: invocation_name.to_string(),
            source,
        })?;
    let run = execute(invocation, &component, manifest_root, host)
        .await
        .map_err(|source| PipelineError::Execute {
            invocation: invocation_name.to_string(),
            source,
        })?;
    Ok(build_cache_entry(
        invocation_name,
        invocation,
        &run,
        options_hash,
    ))
}

fn invocation_id(inv: &Invocation) -> String {
    match &inv.decl_site {
        DeclSite::Manifest { name } => name.clone(),
        DeclSite::Inline { .. } => {
            panic!("inline kiln invocation ids are introduced in M5")
        }
    }
}

fn load_lockfile(manifest_root: &Path) -> Result<wado_manifest::LockFile, PipelineError> {
    let path = manifest_root.join("wado.lock");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(wado_manifest::LockFile {
                version: 1,
                deps_hash: String::new(),
                packages: Vec::new(),
                build_dependencies: Vec::new(),
                generator_cache: Vec::new(),
            });
        }
        Err(source) => {
            return Err(PipelineError::Io { path, source });
        }
    };
    content
        .parse()
        .map_err(|source| PipelineError::LockParse { source })
}

fn save_lockfile(
    manifest_root: &Path,
    lock: &wado_manifest::LockFile,
) -> Result<(), PipelineError> {
    let path = manifest_root.join("wado.lock");
    std::fs::write(&path, lock.to_toml()).map_err(|source| PipelineError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> (Manifest, PathBuf) {
        let m: Manifest = toml_str.parse().expect("valid manifest");
        (m, PathBuf::from("/tmp/does-not-matter"))
    }

    #[test]
    fn empty_manifest_produces_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert!(outcome.plan.order.is_empty());
    }

    #[test]
    fn manifest_without_build_section_is_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[dependencies]
"#,
        );
        assert!(plan(&m, &root).unwrap().plan.order.is_empty());
    }

    #[test]
    fn single_generator_lowered_with_defaults() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 1);
        let inv = &outcome.plan.order[0];
        assert_eq!(inv.from.as_str(), "schema.proto");
        assert_eq!(inv.output_dir.as_str(), "build/kiln/proto");
        assert!(inv.inputs.is_empty());
        match &inv.module {
            GeneratorModule::Spec(s) => assert_eq!(s, "ns:proto@1.0.0"),
            other => panic!("expected Spec, got {other:?}"),
        }
    }

    #[test]
    fn explicit_output_dir_is_preserved() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
output-dir = "gen/proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order[0].output_dir.as_str(), "gen/proto");
    }

    #[test]
    fn two_disjoint_generators_both_scheduled() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }
antlr = { package = "ns:antlr", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./a.proto"

[build.generators.grammar]
module = "ns:antlr@1.0.0"
from = "./a.g4"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 2);
    }

    #[test]
    fn inline_module_record_is_rejected() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[build.generators.proto]
module = { path = "../tools/proto" }
from = "./schema.proto"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        match err {
            DriverError::UnsupportedInlineModule { invocation } => {
                assert_eq!(invocation, "proto");
            }
            other => panic!("expected UnsupportedInlineModule, got {other:?}"),
        }
    }

    #[test]
    fn cycle_between_generators_surfaces_plan_error() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
gen-a = { package = "ns:a", version = "^1.0.0" }
gen-b = { package = "ns:b", version = "^1.0.0" }

[build.generators.first]
module = "ns:a@1.0.0"
from = "build/kiln/second/x.proto"
output-dir = "build/kiln/first"

[build.generators.second]
module = "ns:b@1.0.0"
from = "build/kiln/first/y.proto"
output-dir = "build/kiln/second"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        assert!(matches!(err, DriverError::Plan(PlanError::Cycle { .. })));
    }

    #[test]
    fn provisional_encoder_is_key_order_independent() {
        let a: toml::Value = toml::from_str("x = 1\ny = 2").unwrap();
        let b: toml::Value = toml::from_str("y = 2\nx = 1").unwrap();
        assert_eq!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }

    #[test]
    fn provisional_encoder_distinguishes_string_from_int() {
        let a: toml::Value = toml::from_str("x = \"1\"").unwrap();
        let b: toml::Value = toml::from_str("x = 1").unwrap();
        assert_ne!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }

    #[test]
    fn typed_encoder_byte_identical_to_provisional_on_common_subset() {
        use wado_compiler::kiln::{CanonicalValue, OptionsDescriptor, OptionsField, OptionsType};
        use wado_compiler::token::Span;

        let desc = OptionsDescriptor {
            fields: vec![
                OptionsField {
                    name: "a_bool".to_string(),
                    ty: OptionsType::Bool,
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
                OptionsField {
                    name: "b_int".to_string(),
                    ty: OptionsType::I64,
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
                OptionsField {
                    name: "c_float".to_string(),
                    ty: OptionsType::F64,
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
                OptionsField {
                    name: "d_string".to_string(),
                    ty: OptionsType::String,
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
            ],
        };

        let toml_val: toml::Value = toml::from_str(
            r#"
a_bool = true
b_int = 42
c_float = 3.14
d_string = "hi"
"#,
        )
        .unwrap();
        let provisional_bytes = encode_options_canonical_provisional(&toml_val);

        let attr = toml_to_attr_value(&toml_val);
        let canonical = validate_options(&desc, Some(&attr)).unwrap();
        let typed_bytes = encode_options_canonical(&canonical);

        assert_eq!(
            provisional_bytes, typed_bytes,
            "typed encoder must produce byte-identical output to provisional on the common subset"
        );
        let _ = CanonicalValue::Bool(true);
    }

    #[test]
    fn typed_encoder_option_some_transparent_to_provisional() {
        use wado_compiler::kiln::{OptionsDescriptor, OptionsField, OptionsType};
        use wado_compiler::token::Span;

        let desc = OptionsDescriptor {
            fields: vec![OptionsField {
                name: "rule".to_string(),
                ty: OptionsType::Option(Box::new(OptionsType::String)),
                default: None,
                span: Span::new(0, 0, 1, 1),
            }],
        };

        let toml_val: toml::Value = toml::from_str(r#"rule = "expr""#).unwrap();
        let provisional_bytes = encode_options_canonical_provisional(&toml_val);

        let attr = toml_to_attr_value(&toml_val);
        let canonical = validate_options(&desc, Some(&attr)).unwrap();
        let typed_bytes = encode_options_canonical(&canonical);

        assert_eq!(
            provisional_bytes, typed_bytes,
            "Option::Some(x) must encode identically to a bare x field"
        );
    }

    #[test]
    fn typed_encoder_option_none_omitted_from_table() {
        use wado_compiler::kiln::{OptionsDescriptor, OptionsField, OptionsType};
        use wado_compiler::token::Span;

        let desc = OptionsDescriptor {
            fields: vec![
                OptionsField {
                    name: "name".to_string(),
                    ty: OptionsType::String,
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
                OptionsField {
                    name: "rule".to_string(),
                    ty: OptionsType::Option(Box::new(OptionsType::String)),
                    default: None,
                    span: Span::new(0, 0, 1, 1),
                },
            ],
        };

        let toml_with_none: toml::Value = toml::from_str(r#"name = "x""#).unwrap();
        let provisional_bytes = encode_options_canonical_provisional(&toml_with_none);

        let attr = toml_to_attr_value(&toml_with_none);
        let canonical = validate_options(&desc, Some(&attr)).unwrap();
        let typed_bytes = encode_options_canonical(&canonical);

        assert_eq!(
            provisional_bytes, typed_bytes,
            "Option::None must be omitted from the enclosing table to match TOML omission"
        );
    }

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
                decl_site: DeclSite::Manifest {
                    name: "proto".to_string(),
                },
                module: GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
                from: InvocationPath::normalize("schema.proto"),
                inputs: vec![InvocationPath::normalize("dep.proto")],
                output_dir: InvocationPath::normalize("build/kiln/proto"),
                options_canonical: vec![],
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
        fn unsupported_runner_error_bubbles_up() {
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
            inv.options_canonical = vec![0xde, 0xad, 0xbe, 0xef];

            runtime()
                .block_on(async { execute(&inv, b"wasm", tmp.path(), &host).await })
                .unwrap();
            let reqs = host.requests.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].options, vec![0xde, 0xad, 0xbe, 0xef]);
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
                decl_site: DeclSite::Manifest {
                    name: "proto".to_string(),
                },
                module: GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
                from: InvocationPath::normalize("schema.proto"),
                inputs: vec![InvocationPath::normalize("dep.proto")],
                output_dir: InvocationPath::normalize("build/kiln/proto"),
                options_canonical: vec![],
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
        fn build_cache_entry_round_trips_paths_and_hashes() {
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
            let entry = build_cache_entry(
                "proto",
                &sample_invocation(),
                &run,
                "sha256:optdigest".to_string(),
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
        fn build_cache_entry_sorts_reads_lexicographically() {
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
            let entry =
                build_cache_entry("proto", &sample_invocation(), &run, "sha256:o".to_string());
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
            let entry =
                build_cache_entry("proto", &sample_invocation(), &run, "sha256:o".to_string());
            let host = HashOnlyHost::new(&[("schema.proto", b"p"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host).await
            });
            assert!(hit);
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
            let entry =
                build_cache_entry("proto", &sample_invocation(), &run, "sha256:o".to_string());
            let host = HashOnlyHost::new(&[("schema.proto", b"different"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host).await
            });
            assert!(!hit);
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
            let entry =
                build_cache_entry("proto", &sample_invocation(), &run, "sha256:o".to_string());
            std::fs::remove_file(tmp.path().join("build/kiln/proto/lib.wado")).unwrap();
            let host = HashOnlyHost::new(&[("schema.proto", b"p"), ("dep.proto", b"d")]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host).await
            });
            assert!(!hit);
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
            let entry =
                build_cache_entry("proto", &sample_invocation(), &run, "sha256:o".to_string());
            let host = HashOnlyHost::new(&[
                ("schema.proto", b"p"),
                ("dep.proto", b"d"),
                ("imported.proto", b"mutated"),
            ]);
            let hit = runtime().block_on(async {
                cache_matches(&entry, &sample_invocation(), tmp.path(), &host).await
            });
            assert!(!hit);
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

    mod pipeline_tests {
        use super::*;
        use indexmap::IndexMap;
        use std::sync::Mutex;
        use wado_compiler::compiler_host::{
            Diagnostic, GeneratorOutputFile, GeneratorResponse, GeneratorRunnerError,
        };

        struct StaticProvider {
            bytes: Vec<u8>,
            calls: Mutex<u32>,
        }

        impl StaticProvider {
            fn new(bytes: Vec<u8>) -> Self {
                Self {
                    bytes,
                    calls: Mutex::new(0),
                }
            }
        }

        impl GeneratorProvider for StaticProvider {
            async fn get_component(&self, _: &GeneratorModule) -> Result<Vec<u8>, ProviderError> {
                *self.calls.lock().unwrap() += 1;
                Ok(self.bytes.clone())
            }
        }

        struct FailingProvider;

        impl GeneratorProvider for FailingProvider {
            async fn get_component(&self, _: &GeneratorModule) -> Result<Vec<u8>, ProviderError> {
                Err(ProviderError::Internal {
                    message: "nope".to_string(),
                })
            }
        }

        struct UnsupportedProvider;

        impl GeneratorProvider for UnsupportedProvider {
            async fn get_component(&self, _: &GeneratorModule) -> Result<Vec<u8>, ProviderError> {
                Err(ProviderError::Unsupported {
                    message: "consume-only".to_string(),
                })
            }
        }

        struct PipelineHost {
            sources: IndexMap<String, Vec<u8>>,
            response: Mutex<Vec<Result<GeneratorResponse, GeneratorRunnerError>>>,
            diagnostics: Mutex<Vec<Diagnostic>>,
        }

        impl PipelineHost {
            fn new(
                sources: &[(&str, &[u8])],
                responses: Vec<Result<GeneratorResponse, GeneratorRunnerError>>,
            ) -> Self {
                Self {
                    sources: sources
                        .iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                        .collect(),
                    response: Mutex::new(responses),
                    diagnostics: Mutex::new(Vec::new()),
                }
            }
        }

        impl CompilerHost for PipelineHost {
            async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
                self.sources
                    .get(path)
                    .cloned()
                    .ok_or_else(|| SourceError::NotFound {
                        path: path.to_string(),
                    })
            }
            fn emit_diagnostic(&self, d: Diagnostic) {
                self.diagnostics.lock().unwrap().push(d);
            }
            async fn run_generator(
                &self,
                _wasm: &[u8],
                _request: GeneratorRequest,
            ) -> Result<GeneratorResponse, GeneratorRunnerError> {
                self.response.lock().unwrap().remove(0)
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        fn manifest_with_one_generator() -> Manifest {
            let toml_str = r#"
[package]
name = "host"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.greeter]
module = "ns:proto@1.0.0"
from = "./schema.proto"
"#;
            toml_str.parse().unwrap()
        }

        #[test]
        fn empty_generators_returns_empty_outcome_without_lockfile() {
            let tmp = tempfile::tempdir().unwrap();
            let m: Manifest = "[package]\nname=\"a\"\nversion=\"0.1.0\"".parse().unwrap();
            let host = PipelineHost::new(&[], vec![]);
            let provider = FailingProvider;
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();
            assert!(outcome.cached.is_empty());
            assert!(outcome.executed.is_empty());
            assert!(!tmp.path().join("wado.lock").exists());
        }

        #[test]
        fn first_run_executes_and_writes_lockfile_and_outputs() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = PipelineHost::new(&[("schema.proto", b"schema")], vec![Ok(response)]);
            let provider = StaticProvider::new(b"component-bytes".to_vec());

            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();
            assert_eq!(outcome.executed, vec!["greeter".to_string()]);
            assert!(outcome.cached.is_empty());

            let lib = tmp.path().join("build/kiln/greeter/greeter.wado");
            assert!(lib.exists());

            let lock_content = std::fs::read_to_string(tmp.path().join("wado.lock")).unwrap();
            let lock: wado_manifest::LockFile = lock_content.parse().unwrap();
            assert_eq!(lock.generator_cache.len(), 1);
            assert_eq!(lock.generator_cache[0].invocation, "greeter");
            assert_eq!(lock.generator_cache[0].outputs.len(), 1);
            assert_eq!(*provider.calls.lock().unwrap(), 1);
        }

        #[test]
        fn second_run_with_matching_sources_hits_cache() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };

            let host = PipelineHost::new(&[("schema.proto", b"schema")], vec![Ok(response)]);
            let provider = StaticProvider::new(b"component-bytes".to_vec());
            runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();

            let host2 = PipelineHost::new(&[("schema.proto", b"schema")], vec![]);
            let provider2 = StaticProvider::new(b"component-bytes".to_vec());
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host2, &provider2).await })
                .unwrap();
            assert_eq!(outcome.cached, vec!["greeter".to_string()]);
            assert!(outcome.executed.is_empty());
            assert_eq!(*provider2.calls.lock().unwrap(), 0);
        }

        #[test]
        fn changing_primary_reruns_generator() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let response1 = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host1 = PipelineHost::new(&[("schema.proto", b"original")], vec![Ok(response1)]);
            let provider = StaticProvider::new(b"c".to_vec());
            runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host1, &provider).await })
                .unwrap();

            let response2 = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet_v2() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host2 = PipelineHost::new(&[("schema.proto", b"mutated")], vec![Ok(response2)]);
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host2, &provider).await })
                .unwrap();
            assert_eq!(outcome.executed, vec!["greeter".to_string()]);
        }

        #[test]
        fn pipeline_reconciles_orphan_generated_files() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let out_dir = tmp.path().join("build/kiln/greeter");
            std::fs::create_dir_all(&out_dir).unwrap();
            std::fs::write(
                out_dir.join("leftover.wado"),
                "#![generated(by = \"old\", sources = [])]\n",
            )
            .unwrap();

            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = PipelineHost::new(&[("schema.proto", b"s")], vec![Ok(response)]);
            let provider = StaticProvider::new(b"c".to_vec());
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();
            assert_eq!(
                outcome.deleted,
                vec!["build/kiln/greeter/leftover.wado".to_string()]
            );
            assert!(!out_dir.join("leftover.wado").exists());
            assert!(out_dir.join("greeter.wado").exists());
        }

        #[test]
        fn provider_failure_surfaces_with_invocation_name() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let host = PipelineHost::new(&[("schema.proto", b"s")], vec![]);
            let err = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &FailingProvider).await })
                .unwrap_err();
            match err {
                PipelineError::Provider { invocation, .. } => assert_eq!(invocation, "greeter"),
                other => panic!("expected Provider, got {other:?}"),
            }
        }

        #[test]
        fn consume_only_cache_miss_warns_and_skips_writes() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let host = PipelineHost::new(&[("schema.proto", b"s")], vec![]);
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &UnsupportedProvider).await })
                .unwrap();
            assert_eq!(outcome.stale, vec!["greeter".to_string()]);
            assert!(outcome.executed.is_empty());
            assert!(outcome.cached.is_empty());
            assert!(!tmp.path().join("wado.lock").exists());
            assert!(!tmp.path().join("build/kiln/greeter").exists());
            let diags = host.diagnostics.lock().unwrap();
            assert_eq!(diags.len(), 1);
            assert_eq!(diags[0].code, wado_compiler::Code::KilnStaleCache);
            assert_eq!(diags[0].severity, wado_compiler::Severity::Warning);
        }

        #[test]
        fn consume_only_cache_hit_silent_and_preserves_lockfile() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = PipelineHost::new(&[("schema.proto", b"schema")], vec![Ok(response)]);
            let provider = StaticProvider::new(b"component-bytes".to_vec());
            runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();

            let lock_path = tmp.path().join("wado.lock");
            let original_lock = std::fs::read(&lock_path).unwrap();
            let original_output =
                std::fs::read(tmp.path().join("build/kiln/greeter/greeter.wado")).unwrap();

            let host2 = PipelineHost::new(&[("schema.proto", b"schema")], vec![]);
            let outcome = runtime()
                .block_on(async {
                    run_pipeline(&m, tmp.path(), &host2, &UnsupportedProvider).await
                })
                .unwrap();
            assert_eq!(outcome.cached, vec!["greeter".to_string()]);
            assert!(outcome.stale.is_empty());
            assert!(outcome.executed.is_empty());
            assert!(host2.diagnostics.lock().unwrap().is_empty());
            assert_eq!(std::fs::read(&lock_path).unwrap(), original_lock);
            assert_eq!(
                std::fs::read(tmp.path().join("build/kiln/greeter/greeter.wado")).unwrap(),
                original_output,
            );
        }

        #[test]
        fn consume_only_cache_miss_preserves_existing_build_kiln() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "greeter.wado".to_string(),
                    content: "pub fn greet() {}\n".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = PipelineHost::new(&[("schema.proto", b"original")], vec![Ok(response)]);
            let provider = StaticProvider::new(b"c".to_vec());
            runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();

            let lock_path = tmp.path().join("wado.lock");
            let original_lock = std::fs::read(&lock_path).unwrap();
            let original_output =
                std::fs::read(tmp.path().join("build/kiln/greeter/greeter.wado")).unwrap();

            let host2 = PipelineHost::new(&[("schema.proto", b"mutated")], vec![]);
            let outcome = runtime()
                .block_on(async {
                    run_pipeline(&m, tmp.path(), &host2, &UnsupportedProvider).await
                })
                .unwrap();
            assert_eq!(outcome.stale, vec!["greeter".to_string()]);
            assert!(outcome.cached.is_empty());
            assert!(outcome.executed.is_empty());
            let diags = host2.diagnostics.lock().unwrap();
            assert_eq!(diags.len(), 1);
            assert_eq!(diags[0].code, wado_compiler::Code::KilnStaleCache);
            assert_eq!(std::fs::read(&lock_path).unwrap(), original_lock);
            assert_eq!(
                std::fs::read(tmp.path().join("build/kiln/greeter/greeter.wado")).unwrap(),
                original_output,
            );
        }

        #[test]
        fn consume_only_runner_unsupported_on_cache_miss_warns() {
            let tmp = tempfile::tempdir().unwrap();
            let m = manifest_with_one_generator();
            let host = PipelineHost::new(
                &[("schema.proto", b"s")],
                vec![Err(GeneratorRunnerError::Unsupported)],
            );
            let provider = StaticProvider::new(b"c".to_vec());
            let outcome = runtime()
                .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider).await })
                .unwrap();
            assert_eq!(outcome.stale, vec!["greeter".to_string()]);
            assert!(!tmp.path().join("wado.lock").exists());
            let diags = host.diagnostics.lock().unwrap();
            assert_eq!(diags.len(), 1);
            assert_eq!(diags[0].code, wado_compiler::Code::KilnStaleCache);
        }
    }
}
