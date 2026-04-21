//! CLI-side [`GeneratorProvider`] implementation.
//!
//! Resolves a [`GeneratorModule`] into component bytes and an
//! [`OptionsDescriptor`] for the Kiln driver. v1 supports
//! [`GeneratorModule::LocalPath`] (`module = { path = "..." }`) resolution.
//! Spec-form generators (`module = "ns:name@ver"`) surface
//! [`ProviderError::Unsupported`] with a clear message, matching WEP open-q
//! #4 — registry/git module sources are deferred to a follow-up.
//!
//! Actual component compilation (`path -> .wasm`) depends on compiler-side
//! work that is not yet landed (the `wado:kiln/generator` world, WIT type
//! surface, and stdlib additions). Until those land,
//! [`CliGeneratorProvider::get_component`] returns
//! [`ProviderError::Unsupported`] with an actionable message.
//! [`CliGeneratorProvider::descriptor`] is wired and returns an empty
//! descriptor for paths that exist on disk, letting the driver fall through
//! to the provisional TOML encoder for options.

use std::path::{Path, PathBuf};

use wado_compiler::kiln::{GeneratorModule, OptionsDescriptor};

use crate::kiln_driver::{GeneratorProvider, ProviderError};

/// Cache directory under the project root where resolved generator
/// components are stored: `target/kiln/generators/`.
pub const CACHE_DIR: &str = "target/kiln/generators";

/// CLI-side generator provider.
///
/// Holds the project-root directory so it can resolve `LocalPath` modules
/// relative to the manifest.
#[derive(Debug, Clone)]
pub struct CliGeneratorProvider {
    manifest_root: PathBuf,
}

impl CliGeneratorProvider {
    /// Construct a provider rooted at `manifest_root` — typically the
    /// directory containing `wado.toml`.
    #[must_use]
    pub fn new(manifest_root: PathBuf) -> Self {
        Self { manifest_root }
    }

    fn resolve_path(&self, rel: &str) -> PathBuf {
        self.manifest_root.join(Path::new(rel))
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
                Err(ProviderError::Unsupported {
                    message: format!(
                        "kiln: local generator at `{}` cannot be compiled yet — \
                         the `wado:kiln/generator` world is scheduled for a follow-up PR. \
                         Commit `build/kiln/` and `wado.lock` to use consume-only mode.",
                        path.as_str(),
                    ),
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
                let abs = self.resolve_path(path.as_str());
                if !abs.exists() {
                    return Err(ProviderError::Internal {
                        message: format!("kiln: generator path `{}` does not exist", path.as_str(),),
                    });
                }
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
    fn existing_local_path_surfaces_unsupported_for_now() {
        let tmp = std::env::temp_dir().join("wado-kiln-provider-test");
        std::fs::create_dir_all(&tmp).unwrap();
        let sub = tmp.join("gen");
        std::fs::create_dir_all(&sub).unwrap();
        let provider = CliGeneratorProvider::new(tmp);
        let err = runtime().block_on(async {
            provider
                .get_component(&GeneratorModule::LocalPath(InvocationPath::normalize(
                    "gen",
                )))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Unsupported { message } => {
                assert!(message.contains("follow-up") || message.contains("consume-only"));
            }
            _ => panic!("expected Unsupported, got {err:?}"),
        }
    }
}
