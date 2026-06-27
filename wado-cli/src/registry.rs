//! A [`DependencyProvider`] for `wado update` that resolves path deps from
//! disk; the registry (OCI) and git backends are not wired yet.

use std::future::{Future, ready};
use std::path::PathBuf;

use wado_manifest::{
    DependencyProvider, GitTagInfo, Manifest, ProviderError, RegistryPackageInfo, Version,
};

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

fn backend_pending(source: String) -> ProviderError {
    ProviderError::NotFound {
        source,
        message: "registry/git backend not wired yet (minimal OCI client pending)".to_string(),
    }
}

impl DependencyProvider for FilesystemProvider {
    fn list_registry_versions(
        &self,
        registry_url: &str,
        package: &str,
    ) -> impl Future<Output = Result<Vec<Version>, ProviderError>> + Send {
        ready(Err(backend_pending(format!("{registry_url}/{package}"))))
    }

    fn fetch_registry_package(
        &self,
        registry_url: &str,
        package: &str,
        version: &Version,
    ) -> impl Future<Output = Result<RegistryPackageInfo, ProviderError>> + Send {
        ready(Err(backend_pending(format!(
            "{registry_url}/{package}@{version}"
        ))))
    }

    fn list_git_tags(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Vec<GitTagInfo>, ProviderError>> + Send {
        ready(Err(backend_pending(url.to_string())))
    }

    fn resolve_git_ref(
        &self,
        url: &str,
        ref_name: &str,
    ) -> impl Future<Output = Result<String, ProviderError>> + Send {
        ready(Err(backend_pending(format!("{url}#{ref_name}"))))
    }

    fn fetch_git_manifest(
        &self,
        url: &str,
        sha: &str,
    ) -> impl Future<Output = Result<Manifest, ProviderError>> + Send {
        ready(Err(backend_pending(format!("{url}@{sha}"))))
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
    fn registry_backend_is_pending() {
        let provider = FilesystemProvider::new(".".into());
        let err = block_on(provider.list_registry_versions("https://wa.dev", "mizchi:brotli"))
            .unwrap_err();
        assert!(matches!(err, ProviderError::NotFound { .. }), "{err:?}");
    }
}
