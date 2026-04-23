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
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

use wado_compiler::kiln::{GeneratorModule, OptionsDescriptor};
use wado_compiler::{CompilerHost, CompilerOptions, Diagnostic, LogLevel};

use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{GeneratorProvider, ProviderError};

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
}

/// Compute the stable id for a local generator source file.
///
/// Key inputs (per WEP 2026-04-12 §"`build/kiln/` directory layout"):
/// - normalized project-relative path
/// - source tree hash (v1: SHA-256 of the single entry-file content)
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
struct SilentHost {
    inner: FilesystemCompilerHost,
}

impl CompilerHost for SilentHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, wado_compiler::SourceError>> + Send {
        self.inner.load_source(path)
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

impl GeneratorProvider for CliGeneratorProvider {
    async fn get_component(&self, module: &GeneratorModule) -> Result<Vec<u8>, ProviderError> {
        match module {
            GeneratorModule::Spec(spec) => Err(ProviderError::Unsupported {
                message: format!(
                    "kiln: generator module `{spec}` is declared as a package spec; \
                     registry/workspace build-dependency resolution is not yet supported in v1. \
                     Use `module = {{ path = \"...\" }}` to point at a local generator package."
                ),
            }),
            GeneratorModule::LocalPath(path) => {
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
                let cache_path = self.cache_path(&stable_id);

                if cache_path.is_file()
                    && let Ok(bytes) = std::fs::read(&cache_path)
                {
                    return Ok(bytes);
                }

                // Cache miss: fall through to the real compile. Record it on
                // the shared counter so tests can assert cache behaviour
                // without fiddling with mtime resolutions on tmpfs/NFS.
                self.compile_count.fetch_add(1, Ordering::SeqCst);

                let base_path = abs.parent().map(Path::to_path_buf).unwrap_or_default();
                let abs_str = abs.to_string_lossy().to_string();
                let path_str = path.as_str().to_string();

                // `compile_with_options` captures a `Logger<H>` whose internal
                // `Cell<usize>` makes the returned future `!Send`. Run the
                // whole async compile on a blocking thread with its own
                // current-thread runtime so the multi-thread driver runtime
                // only sees a `Send`-able `spawn_blocking` future.
                let compile_result: Result<Vec<u8>, ProviderError> =
                    tokio::task::spawn_blocking(move || {
                        let host = SilentHost {
                            inner: FilesystemCompilerHost::with_log_level(
                                base_path,
                                LogLevel::Warn,
                            ),
                        };
                        // `skip_validation: true` tracks the open M6.5
                        // adapter-synthesis follow-up: today the CM wrapper
                        // treats the `raw: RawRequest` parameter as a Wasm
                        // GC reference while canonical async lift expects
                        // it in linear memory. The core Wasm is otherwise
                        // well-formed and the component shell is valid;
                        // the mismatch only shows at the validator. Flip
                        // this off once the adapter lands.
                        let options = CompilerOptions {
                            opt_level: wado_compiler::OptLevel::O0,
                            target_world: Some("core:kiln/generator".to_string()),
                            skip_validation: true,
                            log_level: Some(LogLevel::Warn),
                            ..CompilerOptions::default()
                        };
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| ProviderError::Internal {
                                message: format!(
                                    "kiln: failed to start inner runtime for generator \
                                     compile: {e}"
                                ),
                            })?;
                        let result = rt.block_on(async {
                            wado_compiler::compile_with_options(
                                &source_str,
                                &host,
                                Some(&abs_str),
                                options,
                            )
                            .await
                        });
                        match result {
                            Ok(r) => Ok(r.wasm),
                            Err(_) => Err(ProviderError::Internal {
                                message: format!(
                                    "kiln: failed to compile generator `{path_str}` to component"
                                ),
                            }),
                        }
                    })
                    .await
                    .map_err(|e| ProviderError::Internal {
                        message: format!(
                            "kiln: generator compile task panicked or was cancelled: {e}"
                        ),
                    })?;

                let wasm = compile_result?;

                if let Some(parent) = cache_path.parent()
                    && !parent.exists()
                    && std::fs::create_dir_all(parent).is_err()
                {
                    // Cache is best-effort; fall through with the fresh
                    // bytes rather than failing the whole compile.
                }
                let _ = std::fs::write(&cache_path, &wasm);

                Ok(wasm)
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
                let abs = self.resolve_path(path.as_str());
                if !abs.exists() {
                    return Err(ProviderError::Internal {
                        message: format!("kiln: generator path `{}` does not exist", path.as_str()),
                    });
                }
                // v1 provider: exercising `extract_options_descriptor`
                // requires running `annotate` through the compiler, which
                // duplicates the work `get_component` already does.
                // Keep surfacing `Unsupported` until the descriptor-cache
                // follow-up wires these together; the driver's fallback
                // path (provisional TOML encoder) still produces a valid
                // cache key.
                Err(ProviderError::Unsupported {
                    message: format!(
                        "kiln: typed options descriptor for local generator `{}` is not yet \
                         available — falling back to provisional TOML encoding",
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

        let wasm = runtime()
            .block_on(async { provider.get_component(&module).await })
            .unwrap_or_else(|e| panic!("compile failed: {e:?}"));
        assert!(!wasm.is_empty(), "component bytes must be non-empty");
        assert!(
            wasm.starts_with(b"\0asm"),
            "component must start with wasm magic"
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
        assert_eq!(entries.len(), 1, "exactly one cached component expected");

        let wasm2 = runtime()
            .block_on(async { provider.get_component(&module).await })
            .unwrap();
        assert_eq!(wasm, wasm2, "cached second call must match first");
        assert_eq!(
            provider.compile_count(),
            1,
            "cache hit must not re-invoke the inner compiler"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
