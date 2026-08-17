//! Offline resolution of a manifest's `[dependencies]` to real files.
//!
//! One source of truth for where a dependency lives on disk: the LSP reads the
//! warm cache to answer queries without a network, and `wado-cli` fetches a cold
//! one against the same coordinates, versions, and paths. Everything here is
//! read-only.

use std::path::{Path, PathBuf};

use wado_manifest::{DependencySource, LockFile, Manifest};

/// The file that satisfies one `[dependencies]` entry.
#[derive(Debug, PartialEq, Eq)]
pub enum DependencyEntry {
    /// A source dependency (`path` / `git`): the absolute path of its entry
    /// `.wado` module, compiled into the consuming component.
    Source(PathBuf),
    /// A registry dependency: the absolute path of the prebuilt component in
    /// the warm cache, imported across the Component Model boundary.
    Component(PathBuf),
}

/// Resolve every `[dependencies]` entry of `manifest` against the disk, offline.
///
/// `Err` carries a reason phrased for the `use` site. `workspace` dependencies
/// resolve through the workspace itself and are absent. `manifest_dir` holds the
/// manifest, which `path` entries and `wado.lock` both resolve against.
#[must_use]
pub fn resolve_all(
    manifest: &Manifest,
    manifest_dir: &Path,
) -> Vec<(String, Result<DependencyEntry, String>)> {
    let lock = read_lock(manifest_dir);
    let git = lock.as_ref().map(git_pins).unwrap_or_default();
    let registry = lock.as_ref().map(registry_pins).unwrap_or_default();

    let mut out: Vec<(String, Result<DependencyEntry, String>)> = manifest
        .dependencies
        .iter()
        .filter_map(|(name, dep)| {
            let entry = match &dep.source {
                DependencySource::Path { path, .. } => {
                    package_lib_entry(&manifest_dir.join(path)).map(DependencyEntry::Source)
                }
                DependencySource::Git { url, directory, .. } => {
                    git_dependency_entry(&git, name, url, directory.as_deref())
                        .map(DependencyEntry::Source)
                }
                DependencySource::Registry { .. } => return None,
                DependencySource::Workspace => return None,
            };
            Some((name.clone(), entry))
        })
        .collect();

    out.extend(
        registry_component_needs_locked(manifest, &registry)
            .into_iter()
            .map(|need| match need {
                Ok(need) if need.cache_path.is_file() => {
                    (need.name, Ok(DependencyEntry::Component(need.cache_path)))
                }
                Ok(need) => (
                    need.name,
                    Err(format!(
                        "{:?} is not cached; run `wado fetch`",
                        need.coordinate
                    )),
                ),
                Err((name, reason)) => (name, Err(reason)),
            }),
    );
    out
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
    manifest: &Manifest,
    manifest_dir: &Path,
) -> Vec<Result<RegistryComponentNeed, (String, String)>> {
    let locked = read_lock(manifest_dir)
        .as_ref()
        .map(registry_pins)
        .unwrap_or_default();
    registry_component_needs_locked(manifest, &locked)
}

/// [`registry_component_needs`] against an already-parsed lock.
fn registry_component_needs_locked(
    manifest: &Manifest,
    locked: &std::collections::BTreeMap<String, String>,
) -> Vec<Result<RegistryComponentNeed, (String, String)>> {
    let root = cache_root();
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
                    locked,
                    root.as_deref(),
                )
                .map_err(|reason| (name.clone(), reason)),
            ),
            _ => None,
        })
        .collect()
}

fn registry_component_need(
    manifest: &Manifest,
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
/// The entry module of a git dependency, resolved offline from `wado.lock` + the
/// warm worktree cache. `Ok(entry)` is the checked-out `[package].lib` (honoring
/// `directory`); `Err(reason)` explains why it cannot be placed (no lock pin, no
/// cache root, or a cold worktree pointing the user at `wado fetch`).
fn git_dependency_entry(
    locked: &std::collections::BTreeMap<String, (String, String)>,
    name: &str,
    url: &str,
    directory: Option<&str>,
) -> Result<PathBuf, String> {
    let id = format!("git+{url}/{name}");
    let (version, resolved_ref) = locked
        .get(&id)
        .ok_or_else(|| format!("no `wado.lock` entry for {name:?}; run `wado update`"))?;
    let root =
        cache_root().ok_or_else(|| format!("no cache root for {name:?}; set `WADO_ROOT`"))?;
    let relative = wado_manifest::cache::git_worktree_relative(url, version, resolved_ref)
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

/// Parse `manifest_dir`'s `wado.lock`, or `None` when it is absent or
/// malformed (a cold checkout reads as "nothing pinned").
fn read_lock(manifest_dir: &Path) -> Option<LockFile> {
    std::fs::read_to_string(manifest_dir.join("wado.lock"))
        .ok()?
        .parse::<LockFile>()
        .ok()
}

/// `lock id -> (version, resolved-ref)` for every git `[[package]]` in
/// `manifest_dir`'s `wado.lock`. Empty when no lock is present. Shared so the CLI
/// can materialize the same worktrees the offline index resolves against.
#[must_use]
pub fn locked_git_packages(
    manifest_dir: &Path,
) -> std::collections::BTreeMap<String, (String, String)> {
    read_lock(manifest_dir)
        .as_ref()
        .map(git_pins)
        .unwrap_or_default()
}

fn git_pins(lock: &LockFile) -> std::collections::BTreeMap<String, (String, String)> {
    lock.packages
        .iter()
        .filter_map(|pkg| {
            let resolved = pkg.resolved_ref.clone()?;
            Some((pkg.id.clone(), (pkg.version.clone(), resolved)))
        })
        .collect()
}

/// `lock id -> version` for every registry `[[package]]`. Keyed by the full id
/// so distinct registries never collide.
fn registry_pins(lock: &LockFile) -> std::collections::BTreeMap<String, String> {
    lock.packages
        .iter()
        .map(|pkg| (pkg.id.clone(), pkg.version.clone()))
        .collect()
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
