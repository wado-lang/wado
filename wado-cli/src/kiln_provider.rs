//! CLI-side [`GeneratorProvider`] implementation.
//!
//! Resolves a [`GeneratorModule`] into a [`ResolvedGenerator`] (wasm
//! bytes + options descriptor + source-closure hash) for the Kiln
//! driver. It has two variants:
//!
//! - [`GeneratorModule::LocalPath`] — an inline relative-path `module:
//!   "./generator.wado"`, a directory `module: "../package-gale"` (rewritten to
//!   its `[world]."core:kiln/generator"` entry by
//!   [`crate::compile::rewrite_local_dir_modules`]), or a *path*
//!   `[build-dependencies]` specifier (rewritten by
//!   [`crate::compile::rewrite_build_dep_modules`]). All compile from source.
//! - [`GeneratorModule::Spec`] — a *registry* `[build-dependencies]` specifier
//!   (`module: "wado-lang:gale"` / `module: "lib:gale"`). Its published version
//!   is pulled from the `core-kiln-generator` world sub-path as a prebuilt
//!   component and its options descriptor is recovered from the component WIT
//!   (see [`resolve_spec`] and [`crate::kiln_wit`]). A spec with no matching
//!   build-dependency surfaces [`ProviderError::Unsupported`].
//!
//! For `LocalPath`, `resolve` reads the generator source, consults the
//! on-disk cache at `build/kiln/{generators,metadata}/<stable-id>.*`
//! (gated by `no_cache`), and falls back to a fresh
//! [`wado_compiler::compile_with_options`] run on miss. The compile
//! path always rewrites both the wasm and descriptor caches so a
//! subsequent run sees a warm tree.
//!
//! The driver is responsible for not calling `resolve` more than once
//! per unique module per pipeline run — this provider holds no
//! in-memory artifact state.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use wado_compiler::kiln::{GeneratorModule, InvocationPath, OptionsDescriptor};
use wado_compiler::lexer::lex;
use wado_compiler::token::canonical_token_bytes;
use wado_compiler::{CompilerHost, CompilerOptions, Diagnostic, LogLevel};

use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{GeneratorProvider, ProviderError, ResolvedGenerator};
use crate::oci;

/// Directory under the project root where compiled generator
/// components are cached: `build/kiln/generators/`. See WEP 2026-04-12
/// §"`build/kiln/` directory layout".
pub const CACHE_DIR: &str = "build/kiln/generators";

/// Directory under the project root for generator metadata extracted
/// by the compiler (e.g. serialized `OptionsDescriptor`):
/// `build/kiln/metadata/`. Reserved for the descriptor-cache follow-up.
pub const METADATA_DIR: &str = "build/kiln/metadata";

#[derive(Debug, Clone, Default)]
pub struct RegistryContext {
    /// The project's `[build-dependencies]`, keyed as declared (the
    /// `ns:name` coordinate a `module: "ns:name"` spec resolves against).
    pub build_dependencies: indexmap::IndexMap<String, wado_manifest::Dependency>,
    /// The project's `[registries]` (alias → `oci://…` URL).
    pub registries: indexmap::IndexMap<String, String>,
    /// Locked generator versions from `wado.lock` (`coordinate → version`).
    /// When present, a spec resolves to the pinned version without a registry
    /// version listing.
    pub locked_versions: indexmap::IndexMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CliGeneratorProvider {
    manifest_root: PathBuf,
    compile_count: Arc<AtomicUsize>,
    /// When true, skip reads from the on-disk generator cache. Writes
    /// still happen so the next non-bypass run sees a warm tree.
    no_cache: bool,
    /// `[build-dependencies]` + `[registries]` used to resolve a
    /// [`GeneratorModule::Spec`] (`module: "ns:name"`) against the
    /// registry. Empty for callers that only use local generators.
    registry: RegistryContext,
}

impl CliGeneratorProvider {
    #[must_use]
    pub fn new(manifest_root: PathBuf) -> Self {
        Self {
            manifest_root,
            compile_count: Arc::new(AtomicUsize::new(0)),
            no_cache: false,
            registry: RegistryContext::default(),
        }
    }

    #[must_use]
    pub fn with_no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// Attach the `[build-dependencies]` + `[registries]` a
    /// [`GeneratorModule::Spec`] resolves against.
    #[must_use]
    pub fn with_registry_context(mut self, registry: RegistryContext) -> Self {
        self.registry = registry;
        self
    }

    /// Number of times the inner wado-compiler pipeline has actually
    /// run (cache hits do not contribute). Shared across `Clone`s of
    /// the provider so test code with multiple handles still observes
    /// a single total.
    #[must_use]
    pub fn compile_count(&self) -> usize {
        self.compile_count.load(Ordering::SeqCst)
    }

    fn resolve_path(&self, rel: &str) -> PathBuf {
        self.manifest_root.join(Path::new(rel))
    }

    fn cache_path(&self, stable_id: &str) -> PathBuf {
        self.manifest_root
            .join(CACHE_DIR)
            .join(format!("{stable_id}.wasm"))
    }

    fn descriptor_cache_path(&self, stable_id: &str) -> PathBuf {
        self.manifest_root
            .join(METADATA_DIR)
            .join(format!("{stable_id}.descriptor"))
    }

    fn sources_sidecar_path(&self, stable_id: &str) -> PathBuf {
        self.manifest_root
            .join(CACHE_DIR)
            .join(format!("{stable_id}.sources.json"))
    }

    /// Re-validate the cached generator artefacts at `stable_id`
    /// against the on-disk sources captured by the previous compile,
    /// returning the freshly recomputed `combined_hash` on success and
    /// `None` (= treat as cache miss) on any drift. The hash is
    /// recomputed from the validated entries rather than trusted
    /// verbatim from the sidecar, so a hand-edited sidecar cannot pin
    /// metadata to a value that disagrees with its own `sources` list.
    fn validate_sources_sidecar(&self, stable_id: &str, base: &Path) -> Option<String> {
        let sidecar_path = self.sources_sidecar_path(stable_id);
        let bytes = std::fs::read(&sidecar_path).ok()?;
        let sidecar: GeneratorSourcesSidecar = serde_json::from_slice(&bytes).ok()?;
        if sidecar.version != SIDECAR_VERSION {
            return None;
        }
        let mut validated: Vec<(String, [u8; 32])> = Vec::with_capacity(sidecar.sources.len());
        for entry in &sidecar.sources {
            let abs = base.join(&entry.path);
            let bytes = std::fs::read(&abs).ok()?;
            let recorded = hex32_to_array(&entry.hash)?;
            let actual = hash_source(&entry.path, &bytes);
            if actual != recorded {
                return None;
            }
            validated.push((entry.path.clone(), actual));
        }
        Some(combined_sources_hash(&validated))
    }

    fn read_local_source(
        &self,
        path: &wado_compiler::kiln::InvocationPath,
    ) -> Result<(PathBuf, Vec<u8>, String, String), ProviderError> {
        let abs = self.resolve_path(path.as_str());
        if !abs.exists() {
            return Err(ProviderError::Internal {
                message: format!(
                    "kiln: generator path `{}` does not exist (relative to manifest root {})",
                    path.as_str(),
                    self.manifest_root.display(),
                ),
            });
        }
        // A directory module is resolved to its package's
        // `[world]."core:kiln/generator"` entry up front by
        // `compile::rewrite_local_dir_modules`, so a directory reaching the
        // provider is a package that declared no such entry. Report that
        // instead of letting `std::fs::read` surface a bare "Is a directory".
        if abs.is_dir() {
            return Err(ProviderError::Internal {
                message: format!(
                    "kiln: generator module `{}` is a directory but is not a generator package: \
                     its `wado.toml` is missing or declares no [world].\"core:kiln/generator\" \
                     entry. Point `module` at a generator package directory or a `.wado` file",
                    path.as_str(),
                ),
            });
        }
        let source = std::fs::read(&abs).map_err(|e| ProviderError::Internal {
            message: format!(
                "kiln: failed to read generator source at `{}`: {e}",
                abs.display()
            ),
        })?;
        let source_str = match std::str::from_utf8(&source) {
            Ok(s) => s.to_string(),
            Err(_) => {
                return Err(ProviderError::Internal {
                    message: format!(
                        "kiln: generator source at `{}` is not valid UTF-8",
                        abs.display()
                    ),
                });
            }
        };
        let stable_id = stable_id_for_local(path.as_str(), &source);
        Ok((abs, source, source_str, stable_id))
    }

    /// Run the inner wado-compiler pipeline on the given source and
    /// persist both on-disk artefacts (`<id>.wasm` and, when the
    /// compile produces one, `<id>.descriptor`) plus the sources
    /// sidecar that [`Self::try_read_cache`] needs for freshness
    /// checks on later runs.
    async fn compile_local(
        &self,
        path_str: String,
        abs: PathBuf,
        source_str: String,
        stable_id: String,
    ) -> Result<CompileArtifacts, ProviderError> {
        self.compile_count.fetch_add(1, Ordering::SeqCst);

        let base_path = abs.parent().map(Path::to_path_buf).unwrap_or_default();
        let abs_str = abs.to_string_lossy().to_string();
        let recording_base = base_path.clone();
        let loaded = Arc::new(Mutex::new(Vec::<(String, [u8; 32])>::new()));
        let loaded_for_task = loaded.clone();

        // `compile_with_options` captures a `Logger<H>` whose internal
        // `Cell<usize>` makes the returned future `!Send`. Run the whole
        // async compile on a blocking thread with its own current-thread
        // runtime so the multi-thread driver runtime only sees a `Send`-
        // able `spawn_blocking` future.
        let artifacts: Result<CompileArtifacts, ProviderError> =
            tokio::task::spawn_blocking(move || {
                let host = SilentHost {
                    inner: FilesystemCompilerHost::with_log_level(base_path, LogLevel::Warn),
                    loaded: loaded_for_task,
                };
                let options = CompilerOptions {
                    // `O2` matches the default `wado compile` opt level, so
                    // Kiln-invoked generators see the same code their authors
                    // tested against on the CLI. O0 has uncovered bugs that
                    // are orthogonal to the Kiln wiring (e.g. `package-gale`'s
                    // `parser_gen` SQLite path).
                    opt_level: wado_compiler::OptLevel::O2,
                    target_world: Some("core:kiln/generator".to_string()),
                    skip_validation: false,
                    log_level: Some(LogLevel::Warn),
                    ..CompilerOptions::default()
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| ProviderError::Internal {
                        message: format!(
                            "kiln: failed to start inner runtime for generator compile: {e}"
                        ),
                    })?;
                let result = rt.block_on(async {
                    wado_compiler::compile_with_options(&source_str, &host, Some(&abs_str), options)
                        .await
                });
                match result {
                    Ok(r) => Ok(CompileArtifacts {
                        wasm: r.wasm,
                        descriptor: r.kiln_options_descriptor,
                        // Filled in by `compile_local` after the recording
                        // host's `loaded` queue is drained.
                        source_hash: String::new(),
                    }),
                    Err(_) => Err(ProviderError::Internal {
                        message: format!(
                            "kiln: failed to compile generator `{path_str}` to component"
                        ),
                    }),
                }
            })
            .await
            .map_err(|e| ProviderError::Internal {
                message: format!("kiln: generator compile task panicked or was cancelled: {e}"),
            })?;

        let mut artifacts = artifacts?;

        // Drain the recorded sources, dedup paths (the compiler can request
        // the same module more than once), and persist the closure sidecar
        // alongside the cached WASM. The combined hash is what the kiln
        // driver records in `Metadata::generator_source_hash`.
        let recorded = loaded
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let sources = dedup_sort_sources(make_relative_sources(&recording_base, recorded));
        let combined_hash = combined_sources_hash(&sources);
        artifacts.source_hash.clone_from(&combined_hash);

        // Cache writes are best-effort: if the filesystem refuses we
        // still return the fresh bytes. The next invocation just repeats
        // the compile instead of observing a broken cache state.
        let wasm_path = self.cache_path(&stable_id);
        if let Some(parent) = wasm_path.parent()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&wasm_path, &artifacts.wasm);

        let sidecar = GeneratorSourcesSidecar {
            version: SIDECAR_VERSION,
            sources: sources
                .iter()
                .map(|(path, hash)| SourceEntry {
                    path: path.clone(),
                    hash: hex32(hash),
                })
                .collect(),
            combined_hash,
        };
        let sidecar_path = self.sources_sidecar_path(&stable_id);
        if let Ok(bytes) = serde_json::to_vec_pretty(&sidecar) {
            let _ = std::fs::write(&sidecar_path, &bytes);
        }

        if let Some(ref descriptor) = artifacts.descriptor {
            let desc_path = self.descriptor_cache_path(&stable_id);
            if let Some(parent) = desc_path.parent()
                && !parent.exists()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec_pretty(descriptor) {
                let _ = std::fs::write(&desc_path, &bytes);
            }
        }

        Ok(artifacts)
    }
}

/// Convert the absolute paths the recording host captured into project-
/// relative paths anchored at `base`. Stdlib modules (`core:*`, `wasi:*`)
/// never reach `CompilerHost::load_source`, so the only paths that show
/// up here are filesystem sources the inner compiler asked for. Anything
/// outside `base` (e.g. `../shared/foo.wado` if the project layout puts
/// the generator under a sibling directory, or a remote URL the host
/// happens to fetch) is kept verbatim — its hash still pins content, so
/// invalidation stays correct, but the relative-path layout is lost.
fn make_relative_sources(base: &Path, raw: Vec<(String, [u8; 32])>) -> Vec<(String, [u8; 32])> {
    raw.into_iter()
        .map(|(path, hash)| {
            let p = Path::new(&path);
            let rel = p
                .strip_prefix(base)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(path);
            (rel, hash)
        })
        .collect()
}

/// Provider-internal twin of [`ResolvedGenerator`]. Kept distinct so
/// `compile_local` can fill `source_hash` after the inner pipeline
/// returns (the recording host's `loaded` queue isn't drained until
/// then), without exposing a half-built value to the trait surface.
struct CompileArtifacts {
    wasm: Vec<u8>,
    descriptor: Option<OptionsDescriptor>,
    source_hash: String,
}

/// Compute the stable id (per WEP 2026-04-12 §"`build/kiln/` directory
/// layout"): first 16 hex chars of `SHA-256(path || entry-content)`.
///
/// Intentionally hashes only the entry file — it is the cache *lookup*
/// key, not the freshness key. Freshness comes from
/// [`CliGeneratorProvider::validate_sources_sidecar`], which rehashes
/// every transitively imported `.wado`. Without that sidecar an edit
/// to a dep (e.g. `parser_gen.wado` behind a `generator.wado` entry)
/// would silently keep returning stale wasm.
/// Generator-component ABI generation. The compiled component embeds the
/// `core:kiln` world types, so bump this on any `core:kiln/generator.wit` /
/// `emit_kiln_world_types` change to invalidate caches built against the old
/// ABI even when the generator source is unchanged. `v2`: `raw-request.options`
/// became `list<u8>` (CBOR). `v3`: the world is import-only,
/// `raw-request` is gone, and `generate` takes typed `(primary, inputs,
/// options)` parameters.
const KILN_GENERATOR_ABI_TAG: &[u8] = b"kiln-generator-v3\n";

fn stable_id_for_local(path_str: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(KILN_GENERATOR_ABI_TAG);
    hasher.update(path_str.as_bytes());
    hasher.update(b"\n");
    hasher.update(content);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `CompilerHost` wrapper used by `compile_local` for two reasons:
///
/// 1. Swallow generator-compile diagnostics (info/debug) so the
///    consuming project's stderr isn't flooded with nested compile
///    output; the provider surfaces a single `ProviderError` instead.
/// 2. Record every `load_source` call into `loaded` (path + content
///    hash) so the transitive *load closure* — `.wado` modules **and**
///    raw assets pulled via `#include_bytes` — is persisted into the
///    sidecar, letting an edit to either kind invalidate the wasm
///    cache. The recording is best-effort: duplicate paths from the
///    compiler are collapsed by the dedup step in `compile_local`.
struct SilentHost {
    inner: FilesystemCompilerHost,
    loaded: Arc<Mutex<Vec<(String, [u8; 32])>>>,
}

impl CompilerHost for SilentHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, wado_compiler::SourceError>> + Send {
        let path_owned = path.to_string();
        let loaded = self.loaded.clone();
        let inner_fut = self.inner.load_source(path);
        async move {
            let bytes = inner_fut.await?;
            let hash = hash_source(&path_owned, &bytes);
            if let Ok(mut guard) = loaded.lock() {
                guard.push((path_owned, hash));
            }
            Ok(bytes)
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        // Only surface actual errors; silence info/debug logs so the
        // consuming project's stderr isn't flooded with nested compile
        // output.
        if matches!(
            diagnostic.severity,
            wado_compiler::Severity::Error | wado_compiler::Severity::Warning
        ) {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "  [kiln generator compile] {diagnostic}");
        }
    }
}

/// Build a [`ResolvedGenerator`] from a prebuilt generator component: recover
/// the options descriptor from its WIT and hash the bytes as the source hash.
fn resolved_generator_from_wasm(wasm: Vec<u8>) -> Result<ResolvedGenerator, String> {
    let descriptor = crate::kiln_wit::options_descriptor_from_component(&wasm)?;
    let source_hash = hex32(&sha256_of(&wasm));
    Ok(ResolvedGenerator {
        wasm,
        descriptor,
        source_hash,
    })
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// `true` when `path` looks like a Wado source file. Other extensions
/// (binary blobs from `#include_bytes`, text payloads from
/// `#include_str`, raw `.wat` assets) keep the byte-content hash so a
/// single-byte edit still invalidates the cache.
fn is_wado_source(path: &str) -> bool {
    matches!(path.rsplit_once('.'), Some((_, ext)) if ext.eq_ignore_ascii_case("wado"))
}

/// Source-file hash routed by extension. `.wado` files run through the
/// canonical token-stream encoding so comments, doc comments, and
/// formatting changes do not perturb the hash; everything else falls
/// back to a plain content hash.
///
/// On lex failure we deliberately fall back to the byte hash: the file
/// might be a `.wado` shaped sidecar that is not actually parseable
/// (broken on disk between the cache write and validate), and the
/// downstream cache check will still detect drift via byte-hash
/// inequality.
fn hash_source(path: &str, bytes: &[u8]) -> [u8; 32] {
    if !is_wado_source(path) {
        return sha256_of(bytes);
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return sha256_of(bytes);
    };
    let lex_result = lex(source);
    // If the lexer recovered any error the source is malformed; fall back to
    // the byte hash so we don't bake a half-tokenised stream into the cache.
    if !lex_result.errors.is_empty() {
        return sha256_of(bytes);
    }
    // Canonical token bytes ignore spans and (because the lexer peels
    // comments off into a side channel before returning the token list)
    // every line/block/doc comment.
    let mut buf: Vec<u8> = Vec::with_capacity(bytes.len());
    buf.extend_from_slice(b"wado-token-stream-v1\n");
    for tok in &lex_result.tokens {
        canonical_token_bytes(&mut buf, &tok.kind);
    }
    // Shebang and the `__DATA__` trailer carry semantic content that
    // the parser still sees, so fold them into the hash too.
    if let Some(shebang) = &lex_result.shebang {
        buf.push(b'#');
        buf.extend_from_slice(shebang.as_bytes());
        buf.push(0);
    }
    if let Some(data) = &lex_result.data_section {
        buf.push(b'D');
        buf.extend_from_slice(data.as_bytes());
        buf.push(0);
    }
    sha256_of(&buf)
}

/// Sidecar file persisted next to a cached generator WASM, listing every
/// file the inner compiler loaded while building the component — both
/// `.wado` modules and binary assets routed through
/// `CompilerHost::load_source` (e.g. `#include_bytes` payloads). Stdlib
/// modules (`core:*`, `wasi:*`) are bypassed by the compiler and never
/// appear here. Each entry is a project-relative path + the SHA-256 of
/// the file's bytes when the WASM was produced. The provider re-hashes
/// these files on the next call and rebuilds the WASM if any drift, so
/// an edit to a transitive import (e.g. `parser_gen.wado` for a generator
/// entry at `generator.wado`) correctly invalidates the cache.
///
/// `combined_hash` is the SHA-256 of the canonical encoding of `sources`
/// (sorted lex by path, hex-encoded). It is the value stored in
/// `Metadata::generator_source_hash` so the kiln-output cache check in
/// the driver can reuse the same identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeneratorSourcesSidecar {
    /// Schema version for forward compatibility. Bump on incompatible
    /// changes; mismatched versions are treated as cache-miss.
    version: u32,
    sources: Vec<SourceEntry>,
    combined_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceEntry {
    path: String,
    /// Hex-encoded SHA-256 of the file's bytes at compile time.
    hash: String,
}

/// Bumped together with the `combined_sources_hash` magic below
/// whenever the source-hash inputs change. v2 introduced the canonical
/// token-stream encoding for `.wado` files (see [`hash_source`]) so
/// docstring/whitespace edits no longer perturb the hash; pre-existing
/// v1 sidecars are silently treated as cache misses on read.
const SIDECAR_VERSION: u32 = 2;

fn dedup_sort_sources(mut sources: Vec<(String, [u8; 32])>) -> Vec<(String, [u8; 32])> {
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.dedup_by(|a, b| a.0 == b.0);
    sources
}

fn combined_sources_hash(sources: &[(String, [u8; 32])]) -> String {
    let mut hasher = Sha256::new();
    // v2: per-file hashes for `.wado` sources are now token-stream
    // hashes (see `hash_source`); the magic moves in lockstep with the
    // sidecar version so a downgrade or rebuild against an older
    // compiler cannot silently mix v1 and v2 inputs.
    hasher.update(b"kiln-generator-sources-v2\n");
    for (path, hash) in sources {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(hash);
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hex32_to_array(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The OCI world segment a Kiln generator publishes to (`wado publish` maps
/// the `core:kiln/generator` world to this repository sub-path).
impl CliGeneratorProvider {
    /// Resolve a `module: "ns:name"` / `module: "lib:nick"` registry generator:
    /// look the specifier up in `[build-dependencies]`, pick the version (the
    /// `wado.lock` pin when one exists, else the highest published version
    /// matching the requirement), pull its `core-kiln-generator` component, and
    /// recover its options descriptor from the component WIT.
    ///
    /// A path build-dependency is rewritten to `LocalPath` before it reaches the
    /// provider, so a `Spec` here is always a registry entry.
    ///
    /// The pulled component is cached under `build/kiln/generators/` keyed by the
    /// resolved coordinate — the same location `wado fetch` pre-populates — so a
    /// warm cache (or a fetched tree) short-circuits the registry round-trip. A
    /// published version is immutable, so this is sound. `--no-cache` forces a
    /// re-pull.
    async fn resolve_spec(&self, spec: &str) -> Result<ResolvedGenerator, ProviderError> {
        // The lookup key is the coordinate/nickname; a `@version` suffix pins the
        // version directly.
        let key = crate::build_dep::spec_key(spec);
        let pinned = spec[key.len()..].strip_prefix('@').map(str::to_string);

        let dep = self.registry.build_dependencies.get(key).ok_or_else(|| {
            ProviderError::Unsupported {
                message: format!(
                    "kiln: generator `{spec}` is not declared in [build-dependencies]; \
                     add `\"{key}\" = {{ version = \"...\" }}`"
                ),
            }
        })?;
        let wado_manifest::DependencySource::Registry {
            registry,
            package,
            version,
        } = &dep.source
        else {
            return Err(ProviderError::Unsupported {
                message: format!(
                    "kiln: generator `{spec}` must resolve to a registry or path \
                     [build-dependencies] entry; git and workspace sources are not supported"
                ),
            });
        };

        // Identity is the *resolved* coordinate (`package`), so a coordinate and
        // a `lib:` nickname for the same package share one cache entry and lock
        // pin (WEP "generator identity").
        let stable_id = crate::build_dep::generator_stable_id(package);
        if !self.no_cache
            && let Some(resolved) = self.try_read_spec_cache(&stable_id)
        {
            return Ok(resolved);
        }

        let registry_url = self.registry_url(registry.as_deref())?;
        // Prefer the `wado.lock` pin, then a `@version` spec suffix, then a live
        // registry version listing.
        let version = match self
            .registry
            .locked_versions
            .get(package)
            .cloned()
            .or(pinned)
        {
            Some(v) => v,
            None => crate::build_dep::resolve_generator_version(&registry_url, package, version)
                .await
                .map_err(|message| ProviderError::Unsupported { message })?,
        };

        let reference = oci::world_reference(
            &registry_url,
            package,
            crate::build_dep::GENERATOR_WORLD_SEGMENT,
            &version,
        )
        .map_err(|message| ProviderError::Internal { message })?;
        let wasm = oci::pull_component(&reference)
            .await
            .map_err(|e| ProviderError::Internal {
                message: format!("kiln: fetching generator {reference}: {e}"),
            })?;
        let resolved = resolved_generator_from_wasm(wasm).map_err(|e| ProviderError::Internal {
            message: format!("kiln: reading options of generator {reference}: {e}"),
        })?;
        self.write_spec_cache(&stable_id, &resolved.wasm);
        Ok(resolved)
    }

    /// Resolve a registry alias (`None` → `default`) to its `oci://…` URL.
    fn registry_url(&self, alias: Option<&str>) -> Result<String, ProviderError> {
        let alias = alias.unwrap_or("default");
        self.registry
            .registries
            .get(alias)
            .cloned()
            .ok_or_else(|| ProviderError::Unsupported {
                message: format!("kiln: no `[registries].{alias}` to fetch the generator from"),
            })
    }

    /// Read a cached (or `wado fetch`-populated) generator component and rebuild
    /// its `ResolvedGenerator`. The descriptor is recovered from the component
    /// WIT every time rather than cached separately, so a component written by
    /// `wado fetch` (which does not persist a descriptor sidecar) is a full cache
    /// hit. `None` on a cache miss or an unreadable component.
    fn try_read_spec_cache(&self, stable_id: &str) -> Option<ResolvedGenerator> {
        let wasm = std::fs::read(self.cache_path(stable_id)).ok()?;
        resolved_generator_from_wasm(wasm).ok()
    }

    fn write_spec_cache(&self, stable_id: &str, wasm: &[u8]) {
        let cache_path = self.cache_path(stable_id);
        if let Some(dir) = cache_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cache_path, wasm);
    }

    /// Compile (or read from cache) a generator at a manifest-root-relative
    /// path. Used by `LocalPath` resolution (inline paths and rewritten path
    /// build-dependencies).
    async fn resolve_local(
        &self,
        path: &InvocationPath,
    ) -> Result<ResolvedGenerator, ProviderError> {
        let (abs, _source, source_str, stable_id) = self.read_local_source(path)?;
        let base = abs.parent().map(Path::to_path_buf).unwrap_or_default();
        if !self.no_cache
            && let Some(resolved) = self.try_read_cache(&stable_id, &base)
        {
            return Ok(resolved);
        }
        let artifacts = self
            .compile_local(path.as_str().to_string(), abs, source_str, stable_id)
            .await?;
        Ok(ResolvedGenerator {
            wasm: artifacts.wasm,
            descriptor: artifacts.descriptor,
            source_hash: artifacts.source_hash,
        })
    }
}

impl GeneratorProvider for CliGeneratorProvider {
    async fn resolve(&self, module: &GeneratorModule) -> Result<ResolvedGenerator, ProviderError> {
        match module {
            // A `Spec` reaching the provider is a *registry* build-dependency:
            // a path build-dependency is rewritten to `LocalPath` up front by
            // `compile::rewrite_build_dep_modules`.
            GeneratorModule::Spec(spec) => self.resolve_spec(spec).await,
            GeneratorModule::LocalPath(path) => self.resolve_local(path).await,
        }
    }
}

impl CliGeneratorProvider {
    /// Returns `None` on any cache miss or staleness — the caller
    /// recompiles. The descriptor sidecar is *optional* (generators
    /// without `pub struct Options` never write one), so its absence
    /// is `descriptor: None`, not a miss.
    fn try_read_cache(&self, stable_id: &str, base: &Path) -> Option<ResolvedGenerator> {
        let cache_path = self.cache_path(stable_id);
        if !cache_path.is_file() {
            return None;
        }
        let source_hash = self.validate_sources_sidecar(stable_id, base)?;
        let wasm = std::fs::read(&cache_path).ok()?;
        let descriptor = std::fs::read(self.descriptor_cache_path(stable_id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OptionsDescriptor>(&bytes).ok());
        Some(ResolvedGenerator {
            wasm,
            descriptor,
            source_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wado_compiler::kiln::InvocationPath;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn spec_module_without_build_dependency_surfaces_unsupported() {
        // A `module: "ns:x"` spec with no matching `[build-dependencies]` entry
        // (the provider carries an empty registry context) cannot resolve.
        let provider = CliGeneratorProvider::new(PathBuf::from("/tmp"));
        let err = runtime().block_on(async {
            provider
                .resolve(&GeneratorModule::Spec("ns:x@1.0.0".to_string()))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Unsupported { message } => {
                assert!(
                    message.contains("[build-dependencies]"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn directory_module_reaching_provider_surfaces_internal() {
        // A directory is resolved to its package entry up front by
        // `compile::rewrite_local_dir_modules`; one that still reaches the
        // provider declared no `core:kiln/generator` entry, so the provider
        // reports that instead of a bare "Is a directory" read error.
        let tmp = std::env::temp_dir().join(format!(
            "wado-kiln-provider-dir-module-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let pkg = tmp.join("plain-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("wado.toml"),
            "[package]\nname = \"plain\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./plain-pkg"));
        let err = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap_err();
        match err {
            ProviderError::Internal { message } => {
                assert!(
                    message.contains("is a directory") && message.contains("core:kiln/generator"),
                    "unexpected message: {message}"
                );
            }
            _ => panic!("expected Internal, got {err:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_local_path_surfaces_internal() {
        let provider = CliGeneratorProvider::new(PathBuf::from("/nonexistent"));
        let err = runtime().block_on(async {
            provider
                .resolve(&GeneratorModule::LocalPath(InvocationPath::normalize(
                    "./does-not-exist",
                )))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Internal { message } => {
                assert!(message.contains("does not exist"));
            }
            _ => panic!("expected Internal, got {err:?}"),
        }
    }

    #[test]
    fn stable_id_is_deterministic_and_depends_on_content() {
        let a = stable_id_for_local("./gen.wado", b"hello");
        let b = stable_id_for_local("./gen.wado", b"hello");
        assert_eq!(a, b, "stable-id must be deterministic");
        let c = stable_id_for_local("./gen.wado", b"world");
        assert_ne!(a, c, "content change must change stable-id");
        let d = stable_id_for_local("./other.wado", b"hello");
        assert_ne!(a, d, "path change must change stable-id");
    }

    #[test]
    fn local_path_compiles_and_caches_resolved_generator() {
        let tmp =
            std::env::temp_dir().join(format!("wado-kiln-provider-compile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { Request, Response, Error } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub verbose: bool,\n\
}\n\
\n\
export fn generate(req: Request<Options>) -> Result<Response, Error> {\n\
    let _ = req.options.verbose;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";

        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        assert_eq!(provider.compile_count(), 0);

        let resolved = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap_or_else(|e| panic!("compile failed: {e:?}"));
        assert!(resolved.wasm.starts_with(b"\0asm"), "wasm magic missing");
        assert!(
            !resolved.source_hash.is_empty(),
            "local generators must produce a non-empty source hash"
        );
        let descriptor = resolved
            .descriptor
            .as_ref()
            .expect("Options struct present → descriptor must be Some");
        assert_eq!(descriptor.fields.len(), 1);
        assert_eq!(descriptor.fields[0].name, "verbose");
        assert_eq!(
            provider.compile_count(),
            1,
            "first resolve runs the inner compiler exactly once"
        );

        // The compile-and-write path persists wasm + sources sidecar
        // (+ descriptor); assert they all landed so the next resolve
        // exercises the on-disk cache.
        let cache_dir = tmp.join(CACHE_DIR);
        let entries: Vec<_> = std::fs::read_dir(&cache_dir)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "wasm"))
                .count(),
            1,
            "exactly one cached component .wasm expected"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.path().to_string_lossy().ends_with(".sources.json"))
                .count(),
            1,
            "exactly one cached sources sidecar expected"
        );
        let metadata_dir = tmp.join(METADATA_DIR);
        assert_eq!(
            std::fs::read_dir(&metadata_dir)
                .map(|it| it.filter_map(Result::ok).count())
                .unwrap_or(0),
            1,
            "exactly one cached descriptor expected"
        );

        // Second resolve must hit the on-disk cache and skip the compile.
        // The wasm and source-hash must match byte-for-byte; the
        // descriptor's `span` info is intentionally not persisted
        // through the JSON sidecar (it would drift across edits), so
        // compare descriptor facets rather than the whole struct.
        let resolved2 = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap();
        assert_eq!(resolved.wasm, resolved2.wasm, "wasm must round-trip cache");
        assert_eq!(
            resolved.source_hash, resolved2.source_hash,
            "source_hash must round-trip cache",
        );
        let descriptor2 = resolved2
            .descriptor
            .as_ref()
            .expect("warm descriptor cache must rehydrate");
        assert_eq!(descriptor.fields.len(), descriptor2.fields.len());
        for (a, b) in descriptor.fields.iter().zip(descriptor2.fields.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.ty, b.ty);
            assert_eq!(a.default, b.default);
        }
        assert_eq!(
            provider.compile_count(),
            1,
            "warm on-disk cache must not re-invoke the inner compiler"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_after_source_edit_recompiles() {
        // Regression: keying purely off the entry-file `stable_id`
        // must invalidate when the source changes. Combined with the
        // sidecar's transitive-file hash list, an edit to either the
        // entry or a dep must trigger exactly one fresh compile.
        let tmp = std::env::temp_dir().join(format!(
            "wado-kiln-provider-resolve-edit-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { Request, Response, Error } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub highlight: bool,\n\
    pub depth: i32,\n\
}\n\
\n\
export fn generate(req: Request<Options>) -> Result<Response, Error> {\n\
    let _ = req.options.highlight;\n\
    let _ = req.options.depth;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";
        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        let first = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("first resolve");
        let first_desc = first.descriptor.expect("Options struct → Some(descriptor)");
        assert_eq!(first_desc.fields.len(), 2);
        assert_eq!(first_desc.fields[0].name, "highlight");
        assert_eq!(first_desc.fields[1].name, "depth");
        assert_eq!(provider.compile_count(), 1);

        // Second resolve, no edit: disk cache hits, no recompile.
        let _ = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("second resolve");
        assert_eq!(provider.compile_count(), 1);

        // Source edit → fresh compile.
        let updated_src = generator_src.replace("pub depth: i32,\n", "pub depth: i64,\n");
        std::fs::write(&gen_path, &updated_src).unwrap();
        let third = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("resolve after source edit");
        let third_desc = third.descriptor.expect("Options struct → Some(descriptor)");
        assert_eq!(third_desc.fields.len(), 2);
        assert_eq!(third_desc.fields[1].name, "depth");
        assert_eq!(
            provider.compile_count(),
            2,
            "source edit must re-compile exactly once"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
