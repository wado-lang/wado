//! Workspace-member matching rules.
//!
//! Pure path logic: whether a `[workspace].members` glob covers a directory,
//! and the options every glob in the toolchain is matched with. Locating the
//! governing workspace is a filesystem walk and lives with the host that does
//! it (`wado_lsp::host::discovery`).

use std::path::Path;

use glob::{MatchOptions, Pattern};

pub const MANIFEST_FILENAME: &str = "wado.toml";

/// Glob match options shared by the file walker (`wado-cli`'s discover, which
/// re-exports this) and workspace-member matching, so `members` globs and the
/// `[test].exclude` / `--filter` patterns are interpreted identically.
/// `require_literal_separator: true` makes `*` honour path components
/// (`src/*.wado` matches direct children only; `**` is required to cross
/// directories) — standard shell / `.gitignore` convention.
pub const WALK_MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Whether `member_dir` matches any `members` glob, evaluated against the
/// member path relative to the workspace root. No filesystem walk.
#[must_use]
pub fn workspace_governs(root_dir: &Path, members: &[String], member_dir: &Path) -> bool {
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
    use std::path::PathBuf;

    #[test]
    fn a_members_glob_covers_its_directory() {
        let root = PathBuf::from("/ws");
        let members = ["member".to_string(), "packages/*".to_string()];
        assert!(workspace_governs(&root, &members, &root.join("member")));
        assert!(workspace_governs(&root, &members, &root.join("packages/a")));
    }

    #[test]
    fn an_uncovered_directory_is_not_governed() {
        // A directory under the root but outside `members` inherits nothing.
        let root = PathBuf::from("/ws");
        let members = ["member".to_string()];
        assert!(!workspace_governs(&root, &members, &root.join("outsider")));
    }

    #[test]
    fn a_star_does_not_cross_directories() {
        // `require_literal_separator`: `packages/*` covers direct children only.
        let root = PathBuf::from("/ws");
        let members = ["packages/*".to_string()];
        assert!(!workspace_governs(
            &root,
            &members,
            &root.join("packages/a/nested")
        ));
    }

    #[test]
    fn a_directory_outside_the_root_is_not_governed() {
        assert!(!workspace_governs(
            Path::new("/ws"),
            &["member".to_string()],
            Path::new("/elsewhere/member"),
        ));
    }
}
