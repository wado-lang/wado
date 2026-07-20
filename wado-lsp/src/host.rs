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

/// A registry `[dependencies]` entry resolved to its exact lock-pinned version
/// and shared-cache location — the single source of truth shared by the LSP
/// (which reads the cache offline) and the CLI (which fetches a cold cache).
#[derive(Debug)]
pub struct RegistryComponentNeed {
    /// Manifest dependency key — the specifier the loader looks up.
    pub name: String,
    /// `oci://…` registry URL the component is pulled from.
    pub registry_url: String,
    /// `ns:pkg` coordinate.
    pub coordinate: String,
    /// Exact version pinned by `wado.lock`.
    pub version: String,
    /// Absolute path the component occupies in the shared cache.
    pub cache_path: PathBuf,
}

/// Resolve every registry `[dependencies]` entry to its lock-pinned cache need.
/// `Ok(need)` carries the exact version + cache path (whether or not the file is
/// present); `Err((name, reason))` explains why it cannot be placed offline (no
/// registry in scope, no lock pin, or no cache root). Shared so the LSP index
/// and the CLI fetch derive identical coordinates, versions, and paths.
#[must_use]
pub fn registry_component_needs(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
) -> Vec<Result<RegistryComponentNeed, (String, String)>> {
    let root = cache_root();
    let locked = locked_registry_versions(manifest_dir);
    manifest
        .dependencies
        .iter()
        .filter_map(|(name, dep)| match &dep.source {
            DependencySource::Registry {
                registry, package, ..
            } => Some(
                registry_component_need(
                    manifest,
                    name,
                    registry.as_deref(),
                    package,
                    &locked,
                    root.as_deref(),
                )
                .map_err(|reason| (name.clone(), reason)),
            ),
            _ => None,
        })
        .collect()
}

fn registry_component_need(
    manifest: &wado_manifest::Manifest,
    name: &str,
    registry: Option<&str>,
    package: &str,
    locked: &std::collections::BTreeMap<String, String>,
    cache_root: Option<&Path>,
) -> Result<RegistryComponentNeed, String> {
    let alias = registry.unwrap_or("default");
    let registry_url = manifest
        .registries
        .get(alias)
        .ok_or_else(|| format!("no `[registries].{alias}` for {package:?}"))?;
    // Match by the full lock id (`registry+<url>/<coordinate>`), not the bare
    // coordinate, so the same package hosted on two registries stays distinct.
    let id = format!("registry+{registry_url}/{package}");
    let version = locked
        .get(&id)
        .ok_or_else(|| format!("no `wado.lock` version for {package:?}; run `wado update`"))?;
    let cache_root =
        cache_root.ok_or_else(|| format!("no cache root for {package:?}; set `WADO_ROOT`"))?;
    let relative =
        wado_manifest::cache::registry_cache_relative(registry_url, package, None, version)
            .ok_or_else(|| format!("cannot place {package:?} in the cache"))?;
    Ok(RegistryComponentNeed {
        name: name.to_string(),
        registry_url: registry_url.clone(),
        coordinate: package.to_string(),
        version: version.clone(),
        cache_path: cache_root.join(relative),
    })
}

/// Build the dependency index from a manifest's `[dependencies]`. A `path`
/// dependency's entry module (its `[package].lib`, or the file itself for a
/// single-`.wado` path dependency) is recorded relative to `base` — the same
/// base `load_source` joins against — so `use { … } from "<name>"` resolves to
/// it. A `registry` dependency is a prebuilt component: its warm `~/wado/` cache
/// path is recorded so the same import resolves offline (a cold cache lands in
/// `unresolved` with a `wado fetch` hint instead of a generic error). `git` and
/// `workspace` are skipped. `manifest_dir` contains the manifest (path deps and
/// the lock resolve against it).
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    let mut index = DependencyIndex::default();
    let base_abs = absolutize(base);
    for (name, dep) in &manifest.dependencies {
        match &dep.source {
            DependencySource::Path { path, .. } => {
                match package_lib_entry(&manifest_dir.join(path)) {
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
            DependencySource::Git { url, directory, .. } => {
                match git_dependency_entry(manifest_dir, name, url, directory.as_deref()) {
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
            DependencySource::Workspace => {}
            // Registry deps are indexed from `registry_component_needs` below,
            // so the lock is read once for the whole manifest.
            DependencySource::Registry { .. } => {}
        }
    }
    for need in registry_component_needs(manifest, manifest_dir) {
        match need {
            Ok(need) if need.cache_path.is_file() => {
                index
                    .components
                    .insert(need.name, need.cache_path.display().to_string());
            }
            Ok(need) => {
                index.unresolved.insert(
                    need.name,
                    format!("{:?} is not cached; run `wado fetch`", need.coordinate),
                );
            }
            Err((name, reason)) => {
                index.unresolved.insert(name, reason);
            }
        }
    }
    index
}

/// The entry module of a git dependency, resolved offline from `wado.lock` + the
/// warm worktree cache. `Ok(entry)` is the checked-out `[package].lib` (honoring
/// `directory`); `Err(reason)` explains why it cannot be placed (no lock pin, no
/// cache root, or a cold worktree pointing the user at `wado fetch`).
fn git_dependency_entry(
    manifest_dir: &Path,
    name: &str,
    url: &str,
    directory: Option<&str>,
) -> Result<PathBuf, String> {
    let id = format!("git+{url}/{name}");
    let (version, resolved_ref) = locked_git_packages(manifest_dir)
        .remove(&id)
        .ok_or_else(|| format!("no `wado.lock` entry for {name:?}; run `wado update`"))?;
    let root =
        cache_root().ok_or_else(|| format!("no cache root for {name:?}; set `WADO_ROOT`"))?;
    let relative = wado_manifest::cache::git_worktree_relative(url, &version, &resolved_ref)
        .ok_or_else(|| format!("cannot place {name:?} in the cache (bad git url {url:?})"))?;
    let worktree_root = root.join(relative);
    // The `.ready` completion marker (written last by `wado-cli`'s materializer)
    // guards against reading a partial worktree mid-materialize; without it, a
    // cold or in-progress worktree points the user at `wado fetch`.
    let mut marker = worktree_root.clone().into_os_string();
    marker.push(".ready");
    if !worktree_root.is_dir() || !Path::new(&marker).is_file() {
        return Err(format!("{name:?} is not cached; run `wado fetch`"));
    }
    let pkg_dir = match directory {
        Some(dir) => worktree_root.join(dir),
        None => worktree_root,
    };
    package_lib_entry(&pkg_dir)
}

/// `lock id -> (version, resolved-ref)` for every git `[[package]]` in
/// `manifest_dir`'s `wado.lock`. Empty when no lock is present. Shared so the CLI
/// can materialize the same worktrees the offline index resolves against.
#[must_use]
pub fn locked_git_packages(
    manifest_dir: &Path,
) -> std::collections::BTreeMap<String, (String, String)> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(manifest_dir.join("wado.lock")) else {
        return out;
    };
    let Ok(lock) = text.parse::<wado_manifest::LockFile>() else {
        return out;
    };
    for pkg in &lock.packages {
        if let Some(sha) = &pkg.resolved_ref {
            out.insert(pkg.id.clone(), (pkg.version.clone(), sha.clone()));
        }
    }
    out
}

/// `lock id -> version` for every registry `[[package]]` in `manifest_dir`'s
/// `wado.lock`. Keyed by the full id so distinct registries never collide.
/// Empty when no lock is present (a cold checkout).
fn locked_registry_versions(manifest_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(manifest_dir.join("wado.lock")) else {
        return out;
    };
    let Ok(lock) = text.parse::<wado_manifest::LockFile>() else {
        return out;
    };
    for pkg in &lock.packages {
        out.insert(pkg.id.clone(), pkg.version.clone());
    }
    out
}

/// The Wado root (dependency cache): `$WADO_ROOT`, else `~/wado` (`$HOME/wado`).
/// `None` when neither resolves — an honest "no cache" (registry deps then read
/// as uncached) rather than a meaningless relative path.
///
/// This reads only the environment, so it stays dependency-light and works on
/// every target (a no-fs wasm build simply sees no env and falls through to
/// `None`). The `$XDG_CONFIG_HOME/wado/config.toml` `root` key is resolved once
/// by the CLI (`wado-cli`), which exports it as `$WADO_ROOT` at startup, so both
/// the CLI and the embedded LSP server observe one configured root here without
/// this crate ever parsing a config file.
#[must_use]
pub fn cache_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("WADO_ROOT").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(root));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join("wado"))
}

/// Walk up from `start` to find the nearest `wado.toml`, returning the parsed
/// manifest and its directory.
fn nearest_manifest(start: &Path) -> Option<(wado_manifest::Manifest, PathBuf)> {
    let mut dir = absolutize(start);
    loop {
        let candidate = dir.join("wado.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let manifest = crate::workspace::resolve_member_manifest(&dir, &text).ok()?;
            return Some((manifest, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The entry module file of a source dependency: the file itself when the path
/// points at a `.wado` file, otherwise the directory's `[package].lib`. The
/// `Err` describes why a dependency has no usable entry. Shared by the path,
/// git (worktree), and single-file inline-git resolution paths so all three
/// agree on how a package's library entry is located.
pub fn package_lib_entry(dep_path: &Path) -> Result<PathBuf, String> {
    if dep_path.extension().is_some_and(|e| e == "wado") {
        return Ok(dep_path.to_path_buf());
    }
    let manifest_path = dep_path.join("wado.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    // Apply `[workspace.package]` inheritance: a dependency that is a workspace
    // member force-inherits `version` and fails a standalone parse.
    let manifest = crate::workspace::resolve_member_manifest(dep_path, &text)
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
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{RegistryComponentNeed, registry_component_need};

    fn manifest_with_registry_dep() -> wado_manifest::Manifest {
        "[package]\nname=\"app\"\nversion=\"0.1.0\"\n\n\
         [registries]\ndefault=\"oci://ghcr.io\"\n\n\
         [dependencies]\n\"wado-lang:cm-catalog\" = { version = \"^0.1\" }\n"
            .parse()
            .unwrap()
    }

    fn locked(version: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(
            "registry+oci://ghcr.io/wado-lang:cm-catalog".to_string(),
            version.to_string(),
        )])
    }

    fn need(
        locked: &BTreeMap<String, String>,
        cache_root: Option<&Path>,
    ) -> Result<RegistryComponentNeed, String> {
        registry_component_need(
            &manifest_with_registry_dep(),
            "wado-lang:cm-catalog",
            None,
            "wado-lang:cm-catalog",
            locked,
            cache_root,
        )
    }

    #[test]
    fn need_uses_the_lock_version_and_ghq_layout() {
        let n = need(&locked("0.1.0"), Some(Path::new("/cache"))).unwrap();
        assert_eq!(n.name, "wado-lang:cm-catalog");
        assert_eq!(n.coordinate, "wado-lang:cm-catalog");
        assert_eq!(n.version, "0.1.0");
        assert_eq!(n.registry_url, "oci://ghcr.io");
        assert_eq!(
            n.cache_path,
            Path::new("/cache/ghcr.io/wado-lang/cm-catalog/0.1.0/component.wasm")
        );
    }

    #[test]
    fn need_matches_by_full_lock_id_not_bare_coordinate() {
        // A different registry hosting the same coordinate must not match.
        let other = BTreeMap::from([(
            "registry+oci://other.io/wado-lang:cm-catalog".to_string(),
            "9.9.9".to_string(),
        )]);
        let err = need(&other, Some(Path::new("/cache"))).unwrap_err();
        assert!(err.contains("wado update"), "{err}");
    }

    #[test]
    fn need_without_lock_pin_asks_for_update() {
        let err = need(&BTreeMap::new(), Some(Path::new("/cache"))).unwrap_err();
        assert!(err.contains("wado update"), "{err}");
    }

    #[test]
    fn need_without_cache_root_asks_for_wado_root() {
        let err = need(&locked("0.1.0"), None).unwrap_err();
        assert!(err.contains("WADO_ROOT"), "{err}");
    }
}
