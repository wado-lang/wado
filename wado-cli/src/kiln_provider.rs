//! CLI-side [`GeneratorProvider`] implementation.
//!
//! Resolves a [`GeneratorModule`] into component bytes and an
//! [`OptionsDescriptor`] for the Kiln driver. v1 supports
//! [`GeneratorModule::LocalPath`] (`module = { path = "..." }`) resolution.
//! Spec-form generators (`module = "ns:name@ver"`) surface
//! [`ProviderError::Unsupported`] with a clear message, matching WEP open-q
//! #4 — registry/git module sources are deferred to a follow-up.
//!
//! For `LocalPath`, `get_component` reads the generator source, compiles it
//! with `target_world = "core:kiln/generator"` via the ordinary
//! [`wado_compiler::compile_with_options`] pipeline, and caches the
//! resulting component bytes at
//! `build/kiln/generators/<stable-id>.wasm`.  Subsequent calls with the
//! same source tree hash hit the cache without re-running codegen.
//!
//! `descriptor` extracts the generator's `pub struct Options` shape via
//! `wado_compiler::kiln::extract_options_descriptor` on each call. The
//! descriptor is tiny enough that caching it on disk was deferred in
//! favour of simpler semantics.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use wado_compiler::kiln::{GeneratorModule, OptionsDescriptor};
use wado_compiler::{CompilerHost, CompilerOptions, Diagnostic, LogLevel};

use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{GeneratorComponent, GeneratorProvider, ProviderError};

/// Directory under the project root where compiled generator
/// components are cached: `build/kiln/generators/`. See WEP 2026-04-12
/// §"`build/kiln/` directory layout".
pub const CACHE_DIR: &str = "build/kiln/generators";

/// Directory under the project root for generator metadata extracted
/// by the compiler (e.g. serialized `OptionsDescriptor`):
/// `build/kiln/metadata/`. Reserved for the descriptor-cache follow-up.
pub const METADATA_DIR: &str = "build/kiln/metadata";

/// CLI-side generator provider.
///
/// Holds the project-root directory so it can resolve `LocalPath` modules
/// relative to the manifest. The `compile_count` counter records how many
/// times the provider has fallen through to an actual generator-package
/// compile (vs. a cache hit); tests use it to assert that the on-disk
/// `build/kiln/generators/<stable-id>.wasm` cache is honored without
/// relying on filesystem-level mtime observation.
#[derive(Debug, Clone)]
pub struct CliGeneratorProvider {
    manifest_root: PathBuf,
    compile_count: Arc<AtomicUsize>,
}

impl CliGeneratorProvider {
    /// Construct a provider rooted at `manifest_root` — typically the
    /// directory containing `wado.toml`.
    #[must_use]
    pub fn new(manifest_root: PathBuf) -> Self {
        Self {
            manifest_root,
            compile_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of times this provider has run the inner wado-compiler
    /// pipeline. Cache hits do not contribute. The counter is shared
    /// across `Clone`s of the provider (backed by `Arc<AtomicUsize>`)
    /// so callers that keep multiple handles still observe the same
    /// total.
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

    /// Re-validate the cached generator artefacts at `stable_id` against
    /// the on-disk sources captured by the previous compile. Returns the
    /// freshly recomputed `combined_hash` when every listed source file
    /// still matches its recorded hash; returns `None` when the sidecar
    /// is missing, malformed, version-mismatched, or any source file has
    /// drifted (or vanished). Callers must treat `None` as a cache miss
    /// and recompile.
    ///
    /// The combined hash is recomputed from the validated entries rather
    /// than trusted verbatim from the sidecar, so a hand-edited sidecar
    /// cannot pin metadata to a value that disagrees with its own
    /// `sources` list.
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
            let actual = sha256_of(&bytes);
            if actual != recorded {
                return None;
            }
            validated.push((entry.path.clone(), actual));
        }
        Some(combined_sources_hash(&validated))
    }

    /// Resolve a `LocalPath` generator source, returning the absolute
    /// path, raw bytes, UTF-8 source string, and stable id. Emits the
    /// same not-found / not-UTF-8 diagnostics for both `get_component`
    /// and `descriptor` code paths.
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

    /// Run the inner wado-compiler pipeline on the given source, then
    /// persist both caches (`build/kiln/generators/<id>.wasm` and, when
    /// the compile produces one, `build/kiln/metadata/<id>.descriptor`).
    /// Whichever of `get_component` or `descriptor` runs the first
    /// compile warms both, so the second call is a pure disk read.
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
        artifacts.source_hash = combined_hash.clone();

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
/// relative paths anchored at `base`. Builtins and other paths that are
/// not descendants of `base` are kept verbatim — they still hash to the
/// same value and remain stable across runs as long as the toolchain is
/// stable, so they participate in invalidation correctly.
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

/// Both artefacts produced by a single invocation of the inner compiler
/// on a kiln generator package: the component bytes, the extracted
/// `Options` descriptor (when extraction succeeded), and the combined
/// hash of every `.wado` source the compiler loaded along the way.
/// `source_hash` is filled by `compile_local` after the inner pipeline
/// returns; it is the hex digest stored in `Metadata::generator_source_hash`.
struct CompileArtifacts {
    wasm: Vec<u8>,
    descriptor: Option<OptionsDescriptor>,
    source_hash: String,
}

/// Compute the stable id for a local generator source file.
///
/// Key inputs (per WEP 2026-04-12 §"`build/kiln/` directory layout"):
/// - normalized project-relative path
/// - SHA-256 of the entry file's bytes
///
/// Note: the stable id intentionally hashes only the entry file. It is
/// the cache *lookup* key, not the cache *freshness* key — `compile_local`
/// writes a `<stable_id>.sources.json` sidecar listing every transitively
/// imported `.wado` (path + hash) and `validate_sources_sidecar` rejects
/// the cached WASM whenever any of those files drifts on disk. Without
/// the sidecar, an edit to e.g. `parser_gen.wado` (a dep of the entry
/// `generator.wado`) would silently reuse stale bytes.
///
/// Returns the first 16 hex chars of the digest, enough to distinguish
/// typical generator files without bloating `build/kiln/generators/`
/// directory listings.
fn stable_id_for_local(path_str: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kiln-generator-v1\n");
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

/// A thin `CompilerHost` wrapper that discards diagnostics emitted by
/// the host — we want to swallow generator-compile diagnostics so they
/// don't mix with the consuming project's own compile output. The
/// provider surfaces a single `ProviderError::Compile` instead.
///
/// Also records every `load_source` call into `loaded` (path + content
/// hash) so the provider can persist the generator's transitive source
/// closure into a sidecar file and invalidate the WASM cache when any
/// of those files drifts on disk. The recording is best-effort: a
/// duplicate `load_source` for the same path appends a duplicate
/// entry, which the dedup step in `compile_local` collapses.
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
            let hash = sha256_of(&bytes);
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

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Sidecar file persisted next to a cached generator WASM, listing every
/// `.wado` source file the inner compiler loaded while building the
/// component. Each entry is a project-relative path + the SHA-256 of the
/// file's bytes when the WASM was produced. The provider re-hashes these
/// files on the next call and rebuilds the WASM if any drift, so an edit
/// to a transitive import (e.g. `parser_gen.wado` for a generator entry
/// at `generator.wado`) correctly invalidates the cache.
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

const SIDECAR_VERSION: u32 = 1;

fn dedup_sort_sources(mut sources: Vec<(String, [u8; 32])>) -> Vec<(String, [u8; 32])> {
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.dedup_by(|a, b| a.0 == b.0);
    sources
}

fn combined_sources_hash(sources: &[(String, [u8; 32])]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kiln-generator-sources-v1\n");
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

impl GeneratorProvider for CliGeneratorProvider {
    async fn get_component(
        &self,
        module: &GeneratorModule,
    ) -> Result<GeneratorComponent, ProviderError> {
        match module {
            GeneratorModule::Spec(spec) => Err(ProviderError::Unsupported {
                message: format!(
                    "kiln: generator module `{spec}` is declared as a package spec; \
                     registry/workspace build-dependency resolution is not yet supported in v1. \
                     Use `module = {{ path = \"...\" }}` to point at a local generator package."
                ),
            }),
            GeneratorModule::LocalPath(path) => {
                let (abs, source, source_str, stable_id) = self.read_local_source(path)?;
                let base = abs.parent().map(Path::to_path_buf).unwrap_or_default();
                let cache_path = self.cache_path(&stable_id);
                // Cache hit only when (a) the WASM blob exists, (b) the
                // sidecar listing the transitive `.wado` closure exists
                // and re-validates against the current on-disk bytes.
                // Without (b) an edit to a transitive import would
                // silently reuse a stale WASM (the entry-file based
                // `stable_id` would still match), so the kiln-output
                // cache further down the pipeline would be checked
                // against an old generator.
                if cache_path.is_file()
                    && let Some(source_hash) = self.validate_sources_sidecar(&stable_id, &base)
                    && let Ok(bytes) = std::fs::read(&cache_path)
                {
                    return Ok(GeneratorComponent { bytes, source_hash });
                }
                let artifacts = self
                    .compile_local(path.as_str().to_string(), abs, source_str, stable_id)
                    .await?;
                let _ = source;
                Ok(GeneratorComponent {
                    bytes: artifacts.wasm,
                    source_hash: artifacts.source_hash,
                })
            }
        }
    }

    async fn descriptor(
        &self,
        module: &GeneratorModule,
    ) -> Result<OptionsDescriptor, ProviderError> {
        match module {
            GeneratorModule::Spec(_) => Err(ProviderError::Unsupported {
                message: "kiln: cannot introspect options for spec-form generators in v1"
                    .to_string(),
            }),
            GeneratorModule::LocalPath(path) => {
                let (abs, _source, source_str, stable_id) = self.read_local_source(path)?;
                let base = abs.parent().map(Path::to_path_buf).unwrap_or_default();
                let desc_path = self.descriptor_cache_path(&stable_id);
                // Same validation rule as `get_component`: a cached
                // descriptor is only honored when the recorded source
                // closure still matches on disk.
                if desc_path.is_file()
                    && self.validate_sources_sidecar(&stable_id, &base).is_some()
                    && let Ok(bytes) = std::fs::read(&desc_path)
                    && let Ok(descriptor) = serde_json::from_slice::<OptionsDescriptor>(&bytes)
                {
                    return Ok(descriptor);
                }
                let artifacts = self
                    .compile_local(path.as_str().to_string(), abs, source_str, stable_id)
                    .await?;
                artifacts
                    .descriptor
                    .ok_or_else(|| ProviderError::Unsupported {
                        message: format!(
                            "kiln: generator `{}` did not expose a `pub struct Options` the \
                             compiler could describe — falling back to provisional TOML encoding",
                            path.as_str(),
                        ),
                    })
            }
        }
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
    fn spec_module_surfaces_unsupported() {
        let provider = CliGeneratorProvider::new(PathBuf::from("/tmp"));
        let err = runtime().block_on(async {
            provider
                .get_component(&GeneratorModule::Spec("ns:x@1.0.0".to_string()))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Unsupported { message } => {
                assert!(message.contains("registry") || message.contains("not yet supported"));
            }
            _ => panic!("expected Unsupported"),
        }
    }

    #[test]
    fn missing_local_path_surfaces_internal() {
        let provider = CliGeneratorProvider::new(PathBuf::from("/nonexistent"));
        let err = runtime().block_on(async {
            provider
                .get_component(&GeneratorModule::LocalPath(InvocationPath::normalize(
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
    fn local_path_compiles_and_caches_component_bytes() {
        let tmp =
            std::env::temp_dir().join(format!("wado-kiln-provider-compile-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { RawRequest, Response, Error, bind_request } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub verbose: bool,\n\
}\n\
\n\
export fn generate(raw: RawRequest) -> Result<Response, Error> {\n\
    let req = match bind_request::<Options>(raw) {\n\
        Ok(r) => r,\n\
        Err(e) => return Result::Err(e),\n\
    };\n\
    let _ = req.options.verbose;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";

        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        assert_eq!(provider.compile_count(), 0);

        let component = runtime()
            .block_on(async { provider.get_component(&module).await })
            .unwrap_or_else(|e| panic!("compile failed: {e:?}"));
        assert!(
            !component.bytes.is_empty(),
            "component bytes must be non-empty"
        );
        assert!(
            component.bytes.starts_with(b"\0asm"),
            "component must start with wasm magic"
        );
        assert!(
            !component.source_hash.is_empty(),
            "local generators must produce a non-empty source hash"
        );
        assert_eq!(
            provider.compile_count(),
            1,
            "first call runs the inner compiler exactly once"
        );

        // Second call should hit the on-disk cache — observed directly via
        // `compile_count` rather than filesystem state.
        let cache_dir = tmp.join(CACHE_DIR);
        let entries: Vec<_> = std::fs::read_dir(&cache_dir)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        let wasm_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "wasm"))
            .collect();
        assert_eq!(
            wasm_entries.len(),
            1,
            "exactly one cached component .wasm expected"
        );
        // The compile-and-write path also persists a sources sidecar
        // alongside the WASM; assert it landed so the next call's
        // sidecar-validation path is exercised.
        let sidecar_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.path().to_string_lossy().ends_with(".sources.json"))
            .collect();
        assert_eq!(
            sidecar_entries.len(),
            1,
            "exactly one cached sources sidecar expected"
        );

        let component2 = runtime()
            .block_on(async { provider.get_component(&module).await })
            .unwrap();
        assert_eq!(component, component2, "cached second call must match first");
        assert_eq!(
            provider.compile_count(),
            1,
            "cache hit must not re-invoke the inner compiler"
        );

        // The first compile also populated the descriptor cache; an
        // explicit descriptor() call must not trigger a second compile.
        let descriptor = runtime()
            .block_on(async { provider.descriptor(&module).await })
            .expect("descriptor() should succeed once the compile cache is warm");
        assert_eq!(descriptor.fields.len(), 1);
        assert_eq!(descriptor.fields[0].name, "verbose");
        assert_eq!(
            provider.compile_count(),
            1,
            "descriptor cache hit after warm component cache must not re-compile"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn descriptor_populates_metadata_cache_and_hits_on_second_call() {
        let tmp = std::env::temp_dir().join(format!(
            "wado-kiln-provider-descriptor-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { RawRequest, Response, Error, bind_request } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub highlight: bool,\n\
    pub depth: i32,\n\
}\n\
\n\
export fn generate(raw: RawRequest) -> Result<Response, Error> {\n\
    let req = match bind_request::<Options>(raw) {\n\
        Ok(r) => r,\n\
        Err(e) => return Result::Err(e),\n\
    };\n\
    let _ = req.options.highlight;\n\
    let _ = req.options.depth;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";
        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        let first = runtime()
            .block_on(async { provider.descriptor(&module).await })
            .expect("first descriptor call");
        assert_eq!(first.fields.len(), 2);
        assert_eq!(first.fields[0].name, "highlight");
        assert_eq!(first.fields[1].name, "depth");
        assert_eq!(provider.compile_count(), 1);

        let metadata_dir = tmp.join(METADATA_DIR);
        let desc_entries: Vec<_> = std::fs::read_dir(&metadata_dir)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert_eq!(
            desc_entries.len(),
            1,
            "exactly one cached descriptor expected"
        );

        // Second call: disk cache hits, no recompile. Spans are not
        // persisted (intentionally — they would drift across edits), so
        // compare the persisted facets instead of full equality.
        let second = runtime()
            .block_on(async { provider.descriptor(&module).await })
            .expect("second descriptor call");
        assert_eq!(second.fields.len(), first.fields.len());
        for (a, b) in second.fields.iter().zip(first.fields.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.ty, b.ty);
            assert_eq!(a.default, b.default);
        }
        assert_eq!(
            provider.compile_count(),
            1,
            "descriptor cache hit must not re-invoke the inner compiler"
        );

        // Editing the source invalidates the stable-id and forces a
        // fresh compile that populates a new descriptor entry.
        let updated_src = generator_src.replace("pub depth: i32,\n", "pub depth: i64,\n");
        std::fs::write(&gen_path, &updated_src).unwrap();

        let third = runtime()
            .block_on(async { provider.descriptor(&module).await })
            .expect("descriptor after source edit");
        assert_eq!(third.fields.len(), 2);
        assert_eq!(third.fields[1].name, "depth");
        assert_eq!(
            provider.compile_count(),
            2,
            "source edit must re-compile exactly once"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
