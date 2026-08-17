//! Reading the workspace off disk: the governing `wado.toml`, `wado.lock`, and
//! what the warm `~/wado` cache holds.
//!
//! [`wado_manifest`] says where a dependency *belongs*; opening a file to find
//! out what is there happens here, inside the host a browser never runs.

use std::path::{Path, PathBuf};

use wado_manifest::dependency::{
    RegistryComponentNeed, git_pins, registry_component_needs_locked, registry_pins,
};
use wado_manifest::workspace::{MANIFEST_FILENAME, workspace_governs};
use wado_manifest::{DependencySource, LockFile, Manifest, ManifestError, read_workspace_members};

/// The nearest ancestor of `start` (inclusive) that contains a `wado.toml`.
/// `start` may name a file or a directory.
///
/// Absolutized first, so **the result is always absolute** — a caller
/// re-anchoring other paths against it must express those in the same frame.
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
/// A manifest that itself declares `[workspace]` is the authority, not a
/// governed member, so it returns `None`.
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
/// `Err` carries a reason phrased for the `use` site. `workspace` entries resolve
/// through the workspace itself and are absent.
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

/// Every registry `[dependencies]` entry with its lock-pinned version and cache
/// path, present or not. Shared so the LSP index and the CLI fetch agree.
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

/// The entry module of a git dependency, from `wado.lock` + the warm worktree
/// cache: the checked-out `[package].lib`, honoring `directory`.
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

/// `lock id -> (version, resolved-ref)` for every git `[[package]]`, so the CLI
/// materializes the same worktrees the offline index resolves against.
#[must_use]
pub fn locked_git_packages(
    manifest_dir: &Path,
) -> std::collections::BTreeMap<String, (String, String)> {
    read_lock(manifest_dir)
        .as_ref()
        .map(git_pins)
        .unwrap_or_default()
}

/// The Wado root (dependency cache): `$WADO_ROOT`, else `~/wado`. `None` when
/// neither resolves, so registry deps read as uncached rather than landing at a
/// meaningless relative path.
///
/// Env only, no config parsing: `wado-cli` resolves
/// `$XDG_CONFIG_HOME/wado/config.toml`'s `root` and exports `$WADO_ROOT` at
/// startup, so the CLI and the embedded LSP server see one root.
#[must_use]
pub fn cache_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("WADO_ROOT").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(root));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join("wado"))
}
/// The entry module of a source dependency: the file itself for a `.wado` path,
/// otherwise the directory's `[package].lib`. Shared by the path, git, and
/// inline-git resolution paths so all three locate an entry the same way.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace root with one `members` entry, and that member's directory.
    fn workspace_with_member(member: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(MANIFEST_FILENAME),
            format!(
                "[workspace]\nmembers = [\"{member}\"]\n\n[workspace.package]\nversion = \"0.4.0\"\n"
            ),
        )
        .unwrap();
        let member_dir = tmp.path().join(member);
        std::fs::create_dir(&member_dir).unwrap();
        (tmp, member_dir)
    }

    #[test]
    fn member_manifest_inherits_version_from_the_workspace() {
        // A member omitting `version` (force-inherited) resolves by applying
        // `[workspace.package]` — the case a standalone parse rejects.
        let (_tmp, member_dir) = workspace_with_member("member");
        let member_toml = "[package]\nname = \"member\"\n";
        std::fs::write(member_dir.join(MANIFEST_FILENAME), member_toml).unwrap();

        let manifest = resolve_member_manifest(&member_dir, member_toml).unwrap();
        assert_eq!(manifest.package.unwrap().version, "0.4.0");
    }

    #[test]
    fn standalone_manifest_parses_without_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = "[package]\nname = \"solo\"\nversion = \"1.0.0\"\n";
        let manifest = resolve_member_manifest(tmp.path(), toml).unwrap();
        assert_eq!(manifest.package.unwrap().name, "solo");
    }

    #[test]
    fn non_member_directory_does_not_inherit() {
        // A directory under a workspace root but not covered by `members` is not
        // governed, so a manifest missing `version` there fails rather than
        // silently inheriting.
        let (tmp, _member_dir) = workspace_with_member("member");
        let outsider = tmp.path().join("outsider");
        std::fs::create_dir(&outsider).unwrap();
        assert!(governing_workspace(&outsider, "[package]\nname = \"x\"\n").is_none());
    }

    #[test]
    fn a_workspace_member_resolves_as_a_path_dependency() {
        // The whole point of the walk: a path dependency pointing at a member
        // must reach its `[package].lib` rather than failing the inheritance.
        let (_tmp, member_dir) = workspace_with_member("member");
        std::fs::write(
            member_dir.join(MANIFEST_FILENAME),
            "[package]\nname = \"member\"\nlib = \"src/lib.wado\"\n",
        )
        .unwrap();

        let entry = package_lib_entry(&member_dir).expect("member resolves");
        assert_eq!(entry, member_dir.join("src/lib.wado"));
    }

    #[test]
    fn a_single_wado_file_is_its_own_entry() {
        assert_eq!(
            package_lib_entry(Path::new("/pkg/solo.wado")).unwrap(),
            PathBuf::from("/pkg/solo.wado"),
        );
    }

    #[test]
    fn a_package_without_a_lib_entry_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(MANIFEST_FILENAME),
            "[package]\nname = \"nolib\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let err = package_lib_entry(tmp.path()).unwrap_err();
        assert!(err.contains("[package].lib"), "{err}");
    }

    #[test]
    fn nearest_manifest_dir_walks_up_to_the_root() {
        let (tmp, member_dir) = workspace_with_member("member");
        let nested = member_dir.join("src");
        std::fs::create_dir(&nested).unwrap();
        // No `wado.toml` in the member, so the walk continues to the root.
        assert_eq!(
            nearest_manifest_dir(&nested.join("main.wado")),
            Some(tmp.path().to_path_buf()),
        );
    }
}
