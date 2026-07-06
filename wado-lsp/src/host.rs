//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_compiler::{CompilerHost, DependencyIndex, Diagnostic, Severity, SourceError};
use wado_manifest::DependencySource;

#[derive(Debug)]
pub struct FilesystemCompilerHost {
    base_path: PathBuf,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A sibling host that loads sources relative to `base_path` but shares
    /// this host's diagnostics buffer, so diagnostics emitted through either
    /// remain visible to `diagnostics()` / `has_errors()`. The Kiln pipeline
    /// uses this to read schemas relative to the manifest root while its
    /// diagnostics still gate the consuming `wado compile` / `wado check`.
    #[must_use]
    pub fn rebased(&self, base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Arc::clone(&self.diagnostics),
        }
    }

    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Append a diagnostic to the collected buffer without emitting it.
    ///
    /// Wrappers call this after performing their own side effects (e.g. stderr
    /// printing) so the buffer remains the single source of truth for
    /// `has_errors` / `diagnostics()`.
    pub fn collect_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

impl CompilerHost for FilesystemCompilerHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read(&full_path).map_err(|e| SourceError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.collect_diagnostic(diagnostic);
    }

    fn dependency_index(&self) -> DependencyIndex {
        let Some((manifest, root)) = nearest_manifest(&self.base_path) else {
            return DependencyIndex::default();
        };
        dependency_index_from(&manifest, &root, &self.base_path)
    }
}

/// Build the dependency index from a manifest's `[dependencies]`. A `path`
/// dependency's entry module (its `[package].lib`, or the file itself for a
/// single-`.wado` path dependency) is recorded relative to `base` — the same
/// base `load_source` joins against — so `use { … } from "<name>"` resolves to
/// it. A `registry` dependency is a prebuilt component: its warm `~/wado/` cache
/// path is recorded so the same import resolves offline (a cold cache lands in
/// `unresolved` with a `wado fetch` hint instead of a generic error). `git` is
/// skipped. `manifest_dir` contains the manifest (path deps and the lock resolve
/// against it).
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    dependency_index_with_cache_root(manifest, manifest_dir, base, &cache_root())
}

/// [`dependency_index_from`] with an explicit cache root, so the offline
/// registry-component lookup is testable without the process-global environment.
fn dependency_index_with_cache_root(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
    cache_root: &Path,
) -> DependencyIndex {
    let mut index = DependencyIndex::default();
    let base_abs = absolutize(base);
    let locked = locked_registry_versions(manifest_dir);
    for (name, dep) in &manifest.dependencies {
        match &dep.source {
            DependencySource::Path { path, .. } => {
                match dependency_entry_path(&manifest_dir.join(path)) {
                    Ok(entry) => {
                        index
                            .resolved
                            .insert(name.clone(), relative_path(&base_abs, &absolutize(&entry)));
                    }
                    Err(reason) => {
                        index.unresolved.insert(name.clone(), reason);
                    }
                }
            }
            DependencySource::Registry {
                registry, package, ..
            } => match registry_component_path(
                manifest,
                registry.as_deref(),
                package,
                &locked,
                cache_root,
            ) {
                Ok(path) if path.is_file() => {
                    index
                        .components
                        .insert(name.clone(), path.display().to_string());
                }
                Ok(_) => {
                    index.unresolved.insert(
                        name.clone(),
                        format!("{package:?} is not cached; run `wado fetch`"),
                    );
                }
                Err(reason) => {
                    index.unresolved.insert(name.clone(), reason);
                }
            },
            _ => {}
        }
    }
    index
}

/// Resolve a registry dependency's warm-cache component path offline: the
/// `[registries]` URL, the `wado.lock` version, and the shared cache layout.
/// `Err` names why an offline path could not be formed (surfaced at the `use`
/// site).
fn registry_component_path(
    manifest: &wado_manifest::Manifest,
    registry: Option<&str>,
    package: &str,
    locked: &std::collections::BTreeMap<String, String>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let alias = registry.unwrap_or("default");
    let registry_url = manifest
        .registries
        .get(alias)
        .ok_or_else(|| format!("no `[registries].{alias}` for {package:?}"))?;
    let version = locked
        .get(package)
        .ok_or_else(|| format!("no `wado.lock` version for {package:?}; run `wado update`"))?;
    let relative =
        wado_manifest::cache::registry_cache_relative(registry_url, package, None, version)
            .ok_or_else(|| format!("cannot place {package:?} in the cache"))?;
    Ok(cache_root.join(relative))
}

/// `coordinate -> version` for the registry `[dependencies]` in `manifest_dir`'s
/// `wado.lock`. Empty when no lock is present (a cold checkout).
fn locked_registry_versions(manifest_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(manifest_dir.join("wado.lock")) else {
        return out;
    };
    let Ok(lock) = text.parse::<wado_manifest::LockFile>() else {
        return out;
    };
    for pkg in &lock.packages {
        // A registry lock id is `registry+<url>/<ns>:<pkg>`; the coordinate is
        // the `<ns>:<pkg>` tail after the last `/`.
        if let Some((_, coordinate)) = pkg.id.rsplit_once('/') {
            out.insert(coordinate.to_string(), pkg.version.clone());
        }
    }
    out
}

/// The dependency cache root: `$WADO_ROOT`, else `~/wado` (`$HOME/wado`). Falls
/// back to a relative `wado` when neither is set, which simply won't match a
/// real cache — an offline miss, not a crash.
fn cache_root() -> PathBuf {
    if let Some(root) = std::env::var_os("WADO_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(root);
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || PathBuf::from("wado"),
            |home| PathBuf::from(home).join("wado"),
        )
}

/// Walk up from `start` to find the nearest `wado.toml`, returning the parsed
/// manifest and its directory.
fn nearest_manifest(start: &Path) -> Option<(wado_manifest::Manifest, PathBuf)> {
    let mut dir = absolutize(start);
    loop {
        let candidate = dir.join("wado.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let manifest: wado_manifest::Manifest = text.parse().ok()?;
            return Some((manifest, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The entry module file of a path dependency: the file itself when the path
/// points at a `.wado` file, otherwise the directory's `[package].lib`. The
/// `Err` describes why a declared dependency has no usable entry.
fn dependency_entry_path(dep_path: &Path) -> Result<PathBuf, String> {
    if dep_path.extension().is_some_and(|e| e == "wado") {
        return Ok(dep_path.to_path_buf());
    }
    let manifest_path = dep_path.join("wado.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: wado_manifest::Manifest = text
        .parse()
        .map_err(|e| format!("invalid {}: {e}", manifest_path.display()))?;
    let lib = manifest.package.and_then(|p| p.lib).ok_or_else(|| {
        format!(
            "{} declares no [package].lib entry",
            manifest_path.display()
        )
    })?;
    Ok(dep_path.join(lib))
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Lexical relative path from directory `from_dir` to file `to_file`. Both
/// must be absolute; symlinks are not resolved.
fn relative_path(from_dir: &Path, to_file: &Path) -> String {
    let from = normalized_components(from_dir);
    let to = normalized_components(to_file);
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(to[common..].iter().cloned());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn normalized_components(p: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir | Component::Prefix(_) => {}
            Component::RootDir => out.push(String::new()),
            Component::ParentDir => {
                if matches!(out.last().map(String::as_str), None | Some("..")) {
                    out.push("..".to_string());
                } else {
                    out.pop();
                }
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::dependency_index_with_cache_root;

    fn manifest_with_registry_dep() -> wado_manifest::Manifest {
        "[package]\nname=\"app\"\nversion=\"0.1.0\"\n\n\
         [registries]\ndefault=\"oci://ghcr.io\"\n\n\
         [dependencies]\n\"wado-lang:cm-catalog\" = { version = \"^0.1\" }\n"
            .parse()
            .unwrap()
    }

    fn write_lock(dir: &std::path::Path, version: &str) {
        std::fs::write(
            dir.join("wado.lock"),
            format!(
                "version = 1\ndeps-hash = \"x\"\n\n[[package]]\n\
                 id = \"registry+oci://ghcr.io/wado-lang:cm-catalog\"\n\
                 version = \"{version}\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn warm_cache_registry_dep_resolves_to_its_component() {
        let dir = std::env::temp_dir().join("wado-lsp-warm-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_lock(&dir, "0.1.0");

        let cache_root = dir.join("cache");
        let component = cache_root.join("ghcr.io/wado-lang/cm-catalog/0.1.0/component.wasm");
        std::fs::create_dir_all(component.parent().unwrap()).unwrap();
        std::fs::write(&component, b"\0asm").unwrap();

        let index = dependency_index_with_cache_root(
            &manifest_with_registry_dep(),
            &dir,
            &dir,
            &cache_root,
        );
        assert_eq!(
            index
                .components
                .get("wado-lang:cm-catalog")
                .map(String::as_str),
            Some(component.display().to_string().as_str())
        );
        assert!(index.unresolved.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cold_cache_registry_dep_is_unresolved_with_a_hint() {
        let dir = std::env::temp_dir().join("wado-lsp-cold-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_lock(&dir, "0.1.0");

        let index = dependency_index_with_cache_root(
            &manifest_with_registry_dep(),
            &dir,
            &dir,
            &dir.join("empty-cache"),
        );
        assert!(index.components.is_empty());
        let reason = index.unresolved.get("wado-lang:cm-catalog").unwrap();
        assert!(reason.contains("wado fetch"), "{reason}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_dep_without_lock_asks_for_update() {
        let dir = std::env::temp_dir().join("wado-lsp-no-lock-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let index = dependency_index_with_cache_root(
            &manifest_with_registry_dep(),
            &dir,
            &dir,
            &dir.join("cache"),
        );
        let reason = index.unresolved.get("wado-lang:cm-catalog").unwrap();
        assert!(reason.contains("wado update"), "{reason}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
