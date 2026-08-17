//! Reading the workspace off disk: the governing `wado.toml`, `wado.lock`, and
//! what the warm `~/wado` cache actually holds.
//!
//! [`wado_manifest`] stays pure — it says where a dependency *belongs*, given a
//! parsed lock. Everything that opens a file to find out lives here, inside the
//! filesystem host, so a host without a filesystem simply never runs it.

use std::path::{Path, PathBuf};

use wado_manifest::dependency::{
    RegistryComponentNeed, git_pins, registry_component_needs_locked, registry_pins,
};
use wado_manifest::workspace::{MANIFEST_FILENAME, workspace_governs};
use wado_manifest::{DependencySource, LockFile, Manifest, ManifestError, read_workspace_members};

/// The nearest ancestor of `start` (inclusive) that contains a `wado.toml`.
/// `start` may name a file or a directory.
///
/// Absolutized first — a relative path's parent chain runs out after one
/// `pop()` — so **the result is always absolute**. Callers re-anchoring other
/// paths against it must express those in the same frame; every current
/// caller derives its input from an absolute `file:` URI.
#[must_use]
pub fn nearest_manifest_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = absolutize(start);
    loop {
        if dir.join(MANIFEST_FILENAME).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `p` against the current directory when relative, or `p` itself when the
/// process has no readable current directory.
#[must_use]
pub fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Parse a member's `wado.toml`, applying `[workspace.package]` inheritance when
/// `member_dir` belongs to a workspace; otherwise parse it standalone.
///
/// # Errors
/// Propagates TOML, inheritance, and validation errors for the merged member.
pub fn resolve_member_manifest(
    member_dir: &Path,
    member_content: &str,
) -> Result<Manifest, ManifestError> {
    match governing_workspace(member_dir, member_content) {
        Some((_, root_content)) => wado_manifest::resolve_member(member_content, &root_content),
        None => member_content.parse(),
    }
}

/// The workspace governing `member_dir` — its root directory and `wado.toml`
/// contents — if `member_dir` is a member of one.
///
/// A manifest that itself declares `[workspace]` is the workspace authority, not
/// a governed member, so it returns `None`. Otherwise walk up to the nearest
/// ancestor whose `[workspace].members` glob covers the member.
#[must_use]
pub fn governing_workspace(member_dir: &Path, member_content: &str) -> Option<(PathBuf, String)> {
    if read_workspace_members(member_content).is_some() {
        return None;
    }
    let mut dir = member_dir.to_path_buf();
    while dir.pop() {
        let candidate = dir.join(MANIFEST_FILENAME);
        if !candidate.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        if let Some(members) = read_workspace_members(&content)
            && workspace_governs(&dir, &members, member_dir)
        {
            return Some((dir, content));
        }
    }
    None
}

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
        registry_component_needs_locked(manifest, &registry, cache_root().as_deref())
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
    registry_component_needs_locked(manifest, &locked, cache_root().as_deref())
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
    let manifest = resolve_member_manifest(dep_path, &text)
        .map_err(|e| format!("invalid {}: {e}", manifest_path.display()))?;
    let lib = manifest.package.and_then(|p| p.lib).ok_or_else(|| {
        format!(
            "{} declares no [package].lib entry",
            manifest_path.display()
        )
    })?;
    Ok(dep_path.join(lib))
}
