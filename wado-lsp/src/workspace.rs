//! Workspace-member discovery and manifest inheritance.
//!
//! A workspace member's `wado.toml` force-inherits fields (e.g. `version`) from
//! the workspace root's `[workspace.package]`, so loading it standalone fails.
//! These helpers locate the governing workspace and apply inheritance, so a
//! member is loadable as a path dependency — both by the LSP host's
//! dependency-index builder (`host::package_lib_entry`) and by `wado update`'s
//! resolver (`wado-cli` delegates here).

use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};
use wado_manifest::{Manifest, ManifestError, read_workspace_members};

const MANIFEST_FILENAME: &str = "wado.toml";

/// Glob options matching the CLI walker: `*` honours path components, `**` is
/// required to cross directories.
const WALK_MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

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

/// Whether `member_dir` matches any `members` glob, evaluated as a pure path
/// match (no filesystem walk) against the member path relative to the workspace
/// root.
fn workspace_governs(root_dir: &Path, members: &[String], member_dir: &Path) -> bool {
    let Ok(rel) = member_dir.strip_prefix(root_dir) else {
        return false;
    };
    members.iter().any(|pattern| {
        Pattern::new(pattern).is_ok_and(|p| p.matches_path_with(rel, WALK_MATCH_OPTIONS))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_manifest_inherits_version_from_the_workspace() {
        // A member omitting `version` (force-inherited) resolves by applying
        // `[workspace.package]` — the case a standalone parse rejects.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wado.toml"),
            "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\nversion = \"0.4.0\"\n",
        )
        .unwrap();
        let member_dir = tmp.path().join("member");
        std::fs::create_dir(&member_dir).unwrap();
        let member_toml = "[package]\nname = \"member\"\n";
        std::fs::write(member_dir.join("wado.toml"), member_toml).unwrap();

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
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wado.toml"),
            "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\nversion = \"0.4.0\"\n",
        )
        .unwrap();
        let outsider = tmp.path().join("outsider");
        std::fs::create_dir(&outsider).unwrap();
        assert!(governing_workspace(&outsider, "[package]\nname = \"x\"\n").is_none());
    }
}
