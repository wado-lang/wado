use std::fmt;
use std::future::Future;

use indexmap::IndexMap;

use crate::manifest::Manifest;
use crate::version::Version;

/// Information about a registry package at a specific version.
#[derive(Debug, Clone)]
pub struct RegistryPackageInfo {
    /// The parsed manifest of the package.
    pub manifest: Manifest,
    /// Content hash for integrity verification (e.g., `"sha256:a1b2c3..."`).
    pub integrity: String,
}

/// A version tag from a git repository.
#[derive(Debug, Clone)]
pub struct GitTagInfo {
    /// The semver version extracted from the tag name.
    pub version: Version,
    /// The full commit SHA the tag points to.
    pub sha: String,
}

/// Errors from dependency provider operations.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The requested resource was not found.
    NotFound { source: String, message: String },
    /// A network request failed.
    NetworkError { url: String, message: String },
    /// A local I/O operation failed.
    IoError { path: String, message: String },
    /// The fetched manifest is invalid.
    InvalidManifest { source: String, message: String },
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NotFound { source, message } => {
                write!(f, "not found ({source}): {message}")
            }
            ProviderError::NetworkError { url, message } => {
                write!(f, "network error ({url}): {message}")
            }
            ProviderError::IoError { path, message } => {
                write!(f, "I/O error ({path}): {message}")
            }
            ProviderError::InvalidManifest { source, message } => {
                write!(f, "invalid manifest ({source}): {message}")
            }
        }
    }
}

impl std::error::Error for ProviderError {}

/// Abstraction over dependency I/O operations.
///
/// The resolver uses this trait to fetch package metadata without directly
/// accessing the filesystem or network, enabling in-memory testing and
/// wasm32 compatibility.
pub trait DependencyProvider: Send + Sync {
    /// List available versions for a registry package.
    fn list_registry_versions(
        &self,
        registry_url: &str,
        package: &str,
    ) -> impl Future<Output = Result<Vec<Version>, ProviderError>> + Send;

    /// Fetch metadata for a specific version of a registry package.
    fn fetch_registry_package(
        &self,
        registry_url: &str,
        package: &str,
        version: &Version,
    ) -> impl Future<Output = Result<RegistryPackageInfo, ProviderError>> + Send;

    /// List version tags from a git repository.
    fn list_git_tags(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Vec<GitTagInfo>, ProviderError>> + Send;

    /// Resolve a git ref (branch, tag, or SHA prefix) to a full commit SHA.
    fn resolve_git_ref(
        &self,
        url: &str,
        ref_name: &str,
    ) -> impl Future<Output = Result<String, ProviderError>> + Send;

    /// Fetch the manifest from a git repository at a specific commit.
    fn fetch_git_manifest(
        &self,
        url: &str,
        sha: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send;

    /// Load a manifest from a local filesystem path.
    fn load_path_manifest(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send;
}

/// In-memory implementation of [`DependencyProvider`] for testing.
///
/// All data is pre-populated via builder methods. Operations that query
/// data not previously added return `ProviderError::NotFound`.
pub struct InMemoryDependencyProvider {
    /// `registry_url → package_id → [(version, info)]`
    registry: IndexMap<String, IndexMap<String, Vec<(Version, RegistryPackageInfo)>>>,
    /// `git_url → [tag_info]`
    git_tags: IndexMap<String, Vec<GitTagInfo>>,
    /// `git_url → ref_name → sha`
    git_refs: IndexMap<String, IndexMap<String, String>>,
    /// `git_url → sha → manifest`
    git_manifests: IndexMap<String, IndexMap<String, Manifest>>,
    /// `path → manifest`
    path_manifests: IndexMap<String, Manifest>,
}

impl InMemoryDependencyProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: IndexMap::new(),
            git_tags: IndexMap::new(),
            git_refs: IndexMap::new(),
            git_manifests: IndexMap::new(),
            path_manifests: IndexMap::new(),
        }
    }

    /// Add a registry package version.
    pub fn add_registry_package(
        &mut self,
        registry_url: impl Into<String>,
        package: impl Into<String>,
        version: Version,
        info: RegistryPackageInfo,
    ) {
        self.registry
            .entry(registry_url.into())
            .or_default()
            .entry(package.into())
            .or_default()
            .push((version, info));
    }

    /// Add a git tag.
    pub fn add_git_tag(&mut self, url: impl Into<String>, tag: GitTagInfo) {
        self.git_tags.entry(url.into()).or_default().push(tag);
    }

    /// Add a git ref → SHA mapping.
    pub fn add_git_ref(
        &mut self,
        url: impl Into<String>,
        ref_name: impl Into<String>,
        sha: impl Into<String>,
    ) {
        self.git_refs
            .entry(url.into())
            .or_default()
            .insert(ref_name.into(), sha.into());
    }

    /// Add a manifest at a specific git commit.
    pub fn add_git_manifest(
        &mut self,
        url: impl Into<String>,
        sha: impl Into<String>,
        manifest: Manifest,
    ) {
        self.git_manifests
            .entry(url.into())
            .or_default()
            .insert(sha.into(), manifest);
    }

    /// Add a manifest at a local path.
    pub fn add_path_manifest(&mut self, path: impl Into<String>, manifest: Manifest) {
        self.path_manifests.insert(path.into(), manifest);
    }
}

impl Default for InMemoryDependencyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DependencyProvider for InMemoryDependencyProvider {
    fn list_registry_versions(
        &self,
        registry_url: &str,
        package: &str,
    ) -> impl Future<Output = Result<Vec<Version>, ProviderError>> + Send {
        let result = self
            .registry
            .get(registry_url)
            .and_then(|packages| packages.get(package))
            .map(|entries| entries.iter().map(|(v, _)| v.clone()).collect())
            .ok_or_else(|| ProviderError::NotFound {
                source: format!("{registry_url}/{package}"),
                message: "package not found in registry".to_string(),
            });
        std::future::ready(result)
    }

    fn fetch_registry_package(
        &self,
        registry_url: &str,
        package: &str,
        version: &Version,
    ) -> impl Future<Output = Result<RegistryPackageInfo, ProviderError>> + Send {
        let result = self
            .registry
            .get(registry_url)
            .and_then(|packages| packages.get(package))
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|(v, _)| v == version)
                    .map(|(_, info)| info.clone())
            })
            .ok_or_else(|| ProviderError::NotFound {
                source: format!("{registry_url}/{package}@{version}"),
                message: "version not found".to_string(),
            });
        std::future::ready(result)
    }

    fn list_git_tags(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Vec<GitTagInfo>, ProviderError>> + Send {
        let result = self
            .git_tags
            .get(url)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound {
                source: url.to_string(),
                message: "git repository not found".to_string(),
            });
        std::future::ready(result)
    }

    fn resolve_git_ref(
        &self,
        url: &str,
        ref_name: &str,
    ) -> impl Future<Output = Result<String, ProviderError>> + Send {
        let result = self
            .git_refs
            .get(url)
            .and_then(|refs| refs.get(ref_name).cloned())
            .ok_or_else(|| ProviderError::NotFound {
                source: format!("{url}#{ref_name}"),
                message: "git ref not found".to_string(),
            });
        std::future::ready(result)
    }

    fn fetch_git_manifest(
        &self,
        url: &str,
        sha: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send {
        let result = self
            .git_manifests
            .get(url)
            .and_then(|commits| commits.get(sha).cloned())
            .ok_or_else(|| ProviderError::NotFound {
                source: format!("{url}@{sha}"),
                message: "manifest not found at commit".to_string(),
            });
        std::future::ready(result)
    }

    fn load_path_manifest(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send {
        let result =
            self.path_manifests
                .get(path)
                .cloned()
                .ok_or_else(|| ProviderError::NotFound {
                    source: path.to_string(),
                    message: "path not found".to_string(),
                });
        std::future::ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn run_async_test<F>(future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    }

    fn sample_manifest(name: &str, version: &str) -> Manifest {
        format!(
            r#"
[package]
name = "{name}"
version = "{version}"
lib = "src/lib.wado"
"#,
        )
        .parse()
        .unwrap()
    }

    #[test]
    fn registry_list_versions() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "docs:regex",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: sample_manifest("regex", "1.0.0"),
                    integrity: "sha256:aaa".to_string(),
                },
            );
            provider.add_registry_package(
                "https://wa.dev",
                "docs:regex",
                Version::parse("1.1.0").unwrap(),
                RegistryPackageInfo {
                    manifest: sample_manifest("regex", "1.1.0"),
                    integrity: "sha256:bbb".to_string(),
                },
            );

            let versions = provider
                .list_registry_versions("https://wa.dev", "docs:regex")
                .await
                .unwrap();
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].to_string(), "1.0.0");
            assert_eq!(versions[1].to_string(), "1.1.0");
        });
    }

    #[test]
    fn registry_fetch_package() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "docs:regex",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: sample_manifest("regex", "1.0.0"),
                    integrity: "sha256:aaa".to_string(),
                },
            );

            let info = provider
                .fetch_registry_package(
                    "https://wa.dev",
                    "docs:regex",
                    &Version::parse("1.0.0").unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(info.integrity, "sha256:aaa");
            assert_eq!(info.manifest.package.unwrap().name, "regex");
        });
    }

    #[test]
    fn registry_not_found() {
        run_async_test(async {
            let provider = InMemoryDependencyProvider::new();
            let err = provider
                .list_registry_versions("https://wa.dev", "nonexistent:pkg")
                .await
                .unwrap_err();
            assert!(matches!(err, ProviderError::NotFound { .. }));
        });
    }

    #[test]
    fn git_tags() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_tag(
                "https://github.com/user/lib.git",
                GitTagInfo {
                    version: Version::parse("1.0.0").unwrap(),
                    sha: "abc123".to_string(),
                },
            );
            provider.add_git_tag(
                "https://github.com/user/lib.git",
                GitTagInfo {
                    version: Version::parse("1.1.0").unwrap(),
                    sha: "def456".to_string(),
                },
            );

            let tags = provider
                .list_git_tags("https://github.com/user/lib.git")
                .await
                .unwrap();
            assert_eq!(tags.len(), 2);
            assert_eq!(tags[0].sha, "abc123");
            assert_eq!(tags[1].version.to_string(), "1.1.0");
        });
    }

    #[test]
    fn git_resolve_ref() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_ref("https://github.com/user/lib.git", "main", "abc123def456789");

            let sha = provider
                .resolve_git_ref("https://github.com/user/lib.git", "main")
                .await
                .unwrap();
            assert_eq!(sha, "abc123def456789");
        });
    }

    #[test]
    fn git_manifest() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_manifest(
                "https://github.com/user/lib.git",
                "abc123",
                sample_manifest("my-lib", "0.1.0"),
            );

            let manifest = provider
                .fetch_git_manifest("https://github.com/user/lib.git", "abc123")
                .await
                .unwrap();
            let pkg = manifest.package.unwrap();
            assert_eq!(pkg.name, "my-lib");
            assert_eq!(pkg.version, "0.1.0");
        });
    }

    #[test]
    fn path_manifest() {
        run_async_test(async {
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_path_manifest("../shared", sample_manifest("shared", "0.2.0"));

            let manifest = provider.load_path_manifest("../shared").await.unwrap();
            let pkg = manifest.package.unwrap();
            assert_eq!(pkg.name, "shared");
            assert_eq!(pkg.version, "0.2.0");
        });
    }

    #[test]
    fn path_not_found() {
        run_async_test(async {
            let provider = InMemoryDependencyProvider::new();
            let err = provider
                .load_path_manifest("./nonexistent")
                .await
                .unwrap_err();
            assert!(matches!(err, ProviderError::NotFound { .. }));
        });
    }
}
