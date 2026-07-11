//! A [`DependencyProvider`] for `wado update`: path deps from disk and registry
//! deps from an OCI registry (via [`crate::oci`]). The git backend is not wired
//! yet.

use std::future::{Future, ready};
use std::path::PathBuf;

use wado_manifest::{
    DependencyProvider, GitTagInfo, Manifest, ProviderError, RegistryPackageInfo, Version,
};

use crate::oci;

/// Parse an image tag into a semver [`Version`], stripping an optional leading
/// letter prefix (`v1.2.3`, `release1.2.3`) as the manifest WEP's git-tag rule
/// does. Tags that are not semver (e.g. `latest`) yield `None` and are ignored.
pub(crate) fn parse_version_tag(tag: &str) -> Option<Version> {
    if let Ok(version) = Version::parse(tag) {
        return Some(version);
    }
    let rest = tag.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    (rest.len() < tag.len())
        .then(|| Version::parse(rest).ok())
        .flatten()
}

pub struct FilesystemProvider {
    root: PathBuf,
}

impl FilesystemProvider {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn read_path_manifest(&self, path: &str) -> Result<Manifest, ProviderError> {
        let full = self.root.join(path);
        let toml_path = if full.is_dir() {
            full.join("wado.toml")
        } else {
            full
        };
        // A single-file path dep (or a dir without a wado.toml) has no transitive deps.
        if toml_path.file_name().and_then(|n| n.to_str()) != Some("wado.toml")
            || !toml_path.is_file()
        {
            return Ok(crate::compile::empty_manifest());
        }
        let text = std::fs::read_to_string(&toml_path).map_err(|e| ProviderError::IoError {
            path: toml_path.display().to_string(),
            message: e.to_string(),
        })?;
        text.parse::<Manifest>()
            .map_err(|e| ProviderError::InvalidManifest {
                source: toml_path.display().to_string(),
                message: e.to_string(),
            })
    }
}

// Run a blocking git operation on the blocking pool, flattening the join error.
async fn on_blocking<T, F>(f: F) -> Result<T, ProviderError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ProviderError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(ProviderError::IoError {
            path: "git".to_string(),
            message: e.to_string(),
        }),
    }
}

impl DependencyProvider for FilesystemProvider {
    fn list_registry_versions(
        &self,
        registry_url: &str,
        package: &str,
    ) -> impl Future<Output = Result<Vec<Version>, ProviderError>> + Send {
        let registry_url = registry_url.to_string();
        let package = package.to_string();
        async move {
            let source = format!("{registry_url}/{package}");
            let reference =
                oci::reference(&registry_url, &package, "0.0.0").map_err(|message| {
                    ProviderError::NotFound {
                        source: source.clone(),
                        message,
                    }
                })?;
            let tags =
                oci::list_tags(&reference)
                    .await
                    .map_err(|e| ProviderError::NetworkError {
                        url: source,
                        message: e.to_string(),
                    })?;
            Ok(tags.iter().filter_map(|t| parse_version_tag(t)).collect())
        }
    }

    fn fetch_registry_package(
        &self,
        registry_url: &str,
        package: &str,
        version: &Version,
    ) -> impl Future<Output = Result<RegistryPackageInfo, ProviderError>> + Send {
        let registry_url = registry_url.to_string();
        let package = package.to_string();
        let version = version.clone();
        async move {
            let source = format!("{registry_url}/{package}@{version}");
            let reference = oci::reference(&registry_url, &package, &version.to_string()).map_err(
                |message| ProviderError::NotFound {
                    source: source.clone(),
                    message,
                },
            )?;
            let integrity = oci::manifest_digest(&reference).await.map_err(|e| {
                ProviderError::NetworkError {
                    url: source,
                    message: e.to_string(),
                }
            })?;
            // A published Wado package is a standalone Component Model artifact:
            // no transitive Wado dependencies and no source entry module. The
            // lock records the resolved version and the manifest digest.
            Ok(RegistryPackageInfo {
                manifest: crate::compile::empty_manifest(),
                integrity,
            })
        }
    }

    fn list_git_tags(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Vec<GitTagInfo>, ProviderError>> + Send {
        let url = url.to_string();
        on_blocking(move || crate::git::list_tags(&url))
    }

    fn resolve_git_ref(
        &self,
        url: &str,
        ref_name: &str,
    ) -> impl Future<Output = Result<String, ProviderError>> + Send {
        let url = url.to_string();
        let ref_name = ref_name.to_string();
        on_blocking(move || crate::git::resolve_ref(&url, &ref_name))
    }

    fn fetch_git_manifest(
        &self,
        url: &str,
        sha: &str,
        directory: Option<&str>,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send {
        let url = url.to_string();
        let sha = sha.to_string();
        let directory = directory.map(str::to_string);
        on_blocking(move || crate::git::fetch_manifest(&url, &sha, directory.as_deref()))
    }

    fn load_path_manifest(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send {
        ready(self.read_path_manifest(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn path_dep_resolves_to_empty_lock() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wado.toml"),
            r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:shared" = { path = "shared" }
"#,
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join("shared")).unwrap();
        std::fs::write(
            tmp.path().join("shared/wado.toml"),
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let manifest: Manifest = std::fs::read_to_string(tmp.path().join("wado.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let provider = FilesystemProvider::new(tmp.path().to_path_buf());
        let locked = block_on(wado_manifest::resolve(&manifest, &provider)).unwrap();
        assert!(locked.is_empty(), "path deps are not locked: {locked:?}");
    }

    #[test]
    fn non_oci_registry_url_is_rejected() {
        // A registry dependency needs an `oci://` registry; anything else is
        // rejected before any network access.
        let provider = FilesystemProvider::new(".".into());
        let err = block_on(provider.list_registry_versions("https://wa.dev", "mizchi:brotli"))
            .unwrap_err();
        assert!(matches!(err, ProviderError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn parses_semver_tags_and_ignores_others() {
        assert_eq!(parse_version_tag("0.0.9"), Version::parse("0.0.9").ok());
        assert_eq!(parse_version_tag("v1.2.3"), Version::parse("1.2.3").ok());
        assert_eq!(
            parse_version_tag("release2.0.0"),
            Version::parse("2.0.0").ok()
        );
        assert_eq!(parse_version_tag("latest"), None);
        assert_eq!(parse_version_tag("overwrite-test"), None);
    }
}
