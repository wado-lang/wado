//! Test file discovery walker for `wado test` (WEP 2026-05-02).
//!
//! Walks a project root for `*.wado` files, honouring:
//!
//! - `.gitignore` files at any depth (parsed in-process; no `git` binary required)
//! - submodule directories listed in the root `.gitmodules`
//! - dot-prefixed files and directories
//! - subtrees rooted at a nested `wado.toml` (separate package boundaries)
//! - `[test].exclude` glob patterns from the root manifest
//! - symbolic links (followed once each, with cycle detection on canonical paths)

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern, PatternError};

/// Match options used throughout the walker. `require_literal_separator: true`
/// makes `*` honour path components — `src/*.wado` matches direct children
/// only, and `**` is required to cross directories. This matches both
/// standard shell glob convention and `.gitignore` semantics, and keeps the
/// CLI `--filter`, `--exclude`, and `[test].exclude` patterns interpreted
/// the same way.
pub const WALK_MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

#[derive(Debug)]
pub enum WalkError {
    Io(io::Error),
    InvalidExclude {
        pattern: String,
        source: PatternError,
    },
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Io(e) => write!(f, "io error during test discovery: {e}"),
            WalkError::InvalidExclude { pattern, source } => {
                write!(f, "invalid exclude pattern {pattern:?}: {source}")
            }
        }
    }
}

impl std::error::Error for WalkError {}

impl From<io::Error> for WalkError {
    fn from(err: io::Error) -> Self {
        WalkError::Io(err)
    }
}

/// Result of a package-rooted discovery walk.
#[derive(Debug, Default)]
pub struct DiscoveryResult {
    /// `*.wado` files belonging to this package, sorted.
    pub files: Vec<PathBuf>,
    /// Sub-directories that were skipped because they contain their own
    /// `wado.toml`. The caller is expected to recurse into them as separate
    /// package contexts (cargo workspace style; see WEP 2026-05-02).
    pub subpackages: Vec<PathBuf>,
}

/// Discover all `*.wado` files reachable from `root`.
///
/// `excludes` are shell-style globs evaluated against paths relative to `root`.
/// Returned paths are absolute (relative to whatever `root` was given) and
/// sorted for stable output. Returned sub-package roots are sorted as well.
pub fn discover_test_files(root: &Path, excludes: &[String]) -> Result<DiscoveryResult, WalkError> {
    let exclude_patterns = compile_excludes(excludes)?;

    let submodules = read_submodule_paths(root)?
        .into_iter()
        .map(|rel| root.join(rel))
        .collect::<HashSet<_>>();

    let mut visited: HashSet<PathBuf> = HashSet::new();
    if let Ok(canon) = fs::canonicalize(root) {
        visited.insert(canon);
    }

    let mut result = DiscoveryResult::default();
    let mut rules: Vec<GitignoreRule> = Vec::new();
    walk_dir(
        root,
        root,
        &exclude_patterns,
        &submodules,
        &mut rules,
        &mut visited,
        &mut result,
    )?;

    result.files.sort();
    result.subpackages.sort();
    Ok(result)
}

fn compile_excludes(excludes: &[String]) -> Result<Vec<Pattern>, WalkError> {
    let mut out = Vec::with_capacity(excludes.len());
    for s in excludes {
        out.push(
            Pattern::new(s).map_err(|source| WalkError::InvalidExclude {
                pattern: s.clone(),
                source,
            })?,
        );
        // glob's `**` requires at least one trailing path component, so a
        // pattern like `dir/**` does NOT match the bare `dir` entry the
        // walker checks before descending. Users almost always intend
        // `dir/**` to mean "everything at and below dir", so we also accept
        // the `dir` prefix as an exclude. See WEP 2026-05-02.
        if let Some(prefix) = s.strip_suffix("/**")
            && !prefix.is_empty()
        {
            out.push(
                Pattern::new(prefix).map_err(|source| WalkError::InvalidExclude {
                    pattern: s.clone(),
                    source,
                })?,
            );
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    root: &Path,
    excludes: &[Pattern],
    submodules: &HashSet<PathBuf>,
    rules: &mut Vec<GitignoreRule>,
    visited: &mut HashSet<PathBuf>,
    out: &mut DiscoveryResult,
) -> Result<(), WalkError> {
    let added_rules = load_gitignore(dir, rules);

    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();

        if is_dir && submodules.contains(&path) {
            continue;
        }

        if is_ignored(rules, &path, is_dir) {
            continue;
        }

        if let Ok(rel) = path.strip_prefix(root)
            && excludes
                .iter()
                .any(|p| p.matches_path_with(rel, WALK_MATCH_OPTIONS))
        {
            continue;
        }

        if is_dir {
            // Nested wado.toml: separate package boundary; record it for the
            // caller to recurse into and do not enter it ourselves.
            if path.join("wado.toml").is_file() {
                out.subpackages.push(path);
                continue;
            }

            if let Ok(canon) = fs::canonicalize(&path)
                && !visited.insert(canon)
            {
                continue;
            }

            walk_dir(&path, root, excludes, submodules, rules, visited, out)?;
        } else if path.extension().is_some_and(|ext| ext == "wado") {
            out.files.push(path);
        }
    }

    rules.truncate(rules.len() - added_rules);
    Ok(())
}

// ---------------------------------------------------------------------------
// .gitmodules parsing
// ---------------------------------------------------------------------------

fn read_submodule_paths(root: &Path) -> Result<Vec<PathBuf>, WalkError> {
    let path = root.join(".gitmodules");
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(parse_gitmodules(&content))
}

fn parse_gitmodules(content: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("path") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        paths.push(PathBuf::from(rest.trim()));
    }
    paths
}

// ---------------------------------------------------------------------------
// .gitignore parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GitignoreRule {
    pattern: Pattern,
    base: PathBuf,
    dir_only: bool,
    negate: bool,
}

/// Append rules from `dir/.gitignore` (if it exists) to `rules`. Returns the
/// number of rules added so the caller can truncate them after leaving `dir`.
fn load_gitignore(dir: &Path, rules: &mut Vec<GitignoreRule>) -> usize {
    let path = dir.join(".gitignore");
    let Ok(content) = fs::read_to_string(&path) else {
        return 0;
    };
    let added = parse_gitignore(&content, dir);
    let n = added.len();
    rules.extend(added);
    n
}

fn parse_gitignore(content: &str, base: &Path) -> Vec<GitignoreRule> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (negate, body) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };

        let (dir_only, body) = match body.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, body),
        };

        if body.is_empty() {
            continue;
        }

        // Anchored if there is any '/' inside the (already-stripped) body.
        let anchored = body.contains('/');
        let body = body.strip_prefix('/').unwrap_or(body);

        let pattern_str = if anchored {
            body.to_string()
        } else {
            format!("**/{body}")
        };

        if let Ok(pattern) = Pattern::new(&pattern_str) {
            out.push(GitignoreRule {
                pattern,
                base: base.to_path_buf(),
                dir_only,
                negate,
            });
        }
    }
    out
}

fn is_ignored(rules: &[GitignoreRule], path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.dir_only && !is_dir {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&rule.base) else {
            continue;
        };
        if rule.pattern.matches_path_with(rel, WALK_MATCH_OPTIONS) {
            ignored = !rule.negate;
        }
    }
    ignored
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn names_of(root: &Path, paths: &[PathBuf]) -> BTreeSet<String> {
        paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "").unwrap();
    }

    #[test]
    fn discovers_wado_files_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        touch(&root.join("a.wado"));
        touch(&root.join("nested/b.wado"));
        touch(&root.join("nested/deep/c.wado"));
        touch(&root.join("readme.md"));
        touch(&root.join("nested/notes.txt"));

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        let want: BTreeSet<_> = ["a.wado", "nested/b.wado", "nested/deep/c.wado"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn skips_dot_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join(".hidden/secret.wado"));

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        assert_eq!(got.len(), 1);
        assert!(got.contains("a.wado"));
    }

    #[test]
    fn honours_gitignore_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("target/build.wado"));
        fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        assert!(got.contains("a.wado"));
        assert!(!got.iter().any(|p| p.starts_with("target/")));
    }

    #[test]
    fn honours_nested_gitignore_with_negation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("pkg/a.wado"));
        touch(&root.join("pkg/skip/b.wado"));
        touch(&root.join("pkg/skip/keep.wado"));
        fs::write(root.join("pkg/.gitignore"), "skip/\n!skip/keep.wado\n").unwrap();

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        // Negation cannot resurrect files inside an ignored directory because
        // the directory itself is skipped — match git's actual behaviour.
        assert!(got.contains("pkg/a.wado"));
        assert!(!got.contains("pkg/skip/b.wado"));
    }

    #[test]
    fn unanchored_gitignore_pattern_matches_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("nested/a.wado"));
        touch(&root.join("nested/skipme.wado"));
        fs::write(root.join(".gitignore"), "skipme.wado\n").unwrap();

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        assert!(got.contains("a.wado"));
        assert!(got.contains("nested/a.wado"));
        assert!(!got.contains("nested/skipme.wado"));
    }

    #[test]
    fn skips_submodule_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("vendor/sub/lib.wado"));
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"sub\"]\n    path = vendor/sub\n    url = https://example.com\n",
        )
        .unwrap();

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        assert!(got.contains("a.wado"));
        assert!(!got.iter().any(|p| p.starts_with("vendor/sub/")));
    }

    #[test]
    fn skips_nested_wado_toml_packages_and_records_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("subpkg/main.wado"));
        fs::write(
            root.join("subpkg/wado.toml"),
            "[package]\nname=\"sub\"\nversion=\"0.1.0\"\ncommand=\"main.wado\"\n",
        )
        .unwrap();

        let result = discover_test_files(root, &[]).unwrap();
        let got = names_of(root, &result.files);
        assert!(got.contains("a.wado"));
        assert!(!got.contains("subpkg/main.wado"));
        // The nested package directory is reported so the caller can recurse.
        assert_eq!(result.subpackages, vec![root.join("subpkg")]);
    }

    #[test]
    fn applies_manifest_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("src/main.wado"));
        touch(&root.join("compiler/tests/fixture.wado"));

        let files = discover_test_files(root, &["compiler/tests/**".to_string()])
            .unwrap()
            .files;
        let got = names_of(root, &files);
        assert!(got.contains("src/main.wado"));
        assert!(!got.contains("compiler/tests/fixture.wado"));
    }

    #[test]
    fn dir_globstar_pattern_excludes_the_directory_itself() {
        // `glob`'s `**` only matches at least one trailing path component, so
        // `dir/**` alone would NOT keep the walker out of `dir`. The walker
        // augments such patterns with the bare `dir` prefix so users can
        // type `dir/**` and expect "everything at and below dir" excluded.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("keep.wado"));
        touch(&root.join("skip/me.wado"));
        touch(&root.join("skip/deep/also.wado"));

        let result = discover_test_files(root, &["skip/**".to_string()]).unwrap();
        let got = names_of(root, &result.files);
        assert_eq!(got.len(), 1);
        assert!(got.contains("keep.wado"));
    }

    #[test]
    fn dir_globstar_excludes_subpackage_root_too() {
        // The same `dir/**` augmentation must apply when the directory is a
        // sub-package boundary; otherwise a CLI `--exclude package/**` could
        // not stop the test runner from recursing into the sub-package.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("subpkg/main.wado"));
        fs::write(
            root.join("subpkg/wado.toml"),
            "[package]\nname=\"sub\"\nversion=\"0.1.0\"\ncommand=\"main.wado\"\n",
        )
        .unwrap();

        let result = discover_test_files(root, &["subpkg/**".to_string()]).unwrap();
        let got = names_of(root, &result.files);
        assert_eq!(got.len(), 1);
        assert!(got.contains("a.wado"));
        assert!(
            result.subpackages.is_empty(),
            "subpkg should be excluded entirely, got {:?}",
            result.subpackages,
        );
    }

    #[test]
    fn star_matches_within_one_path_component() {
        // Sanity check that `glob`'s `*` honours path separators when set up
        // via the walker. `src/*.wado` should match files directly under
        // `src/` but not `src/sub/*.wado`. We rely on this for [test].exclude
        // semantics.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("src/top.wado"));
        touch(&root.join("src/sub/inner.wado"));

        let files = discover_test_files(root, &["src/*.wado".to_string()])
            .unwrap()
            .files;
        let got = names_of(root, &files);
        assert!(!got.contains("src/top.wado"));
        assert!(got.contains("src/sub/inner.wado"));
    }

    #[test]
    fn double_star_matches_at_any_depth() {
        // `**/foo.wado` is the standard glob shape for "match anywhere".
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a/foo.wado"));
        touch(&root.join("a/b/foo.wado"));
        touch(&root.join("a/b/c/foo.wado"));
        touch(&root.join("a/b/keep.wado"));

        let files = discover_test_files(root, &["**/foo.wado".to_string()])
            .unwrap()
            .files;
        let got = names_of(root, &files);
        assert!(!got.iter().any(|p| p.ends_with("/foo.wado")));
        assert!(got.contains("a/b/keep.wado"));
    }

    #[test]
    fn rejects_invalid_exclude_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let err = discover_test_files(root, &["[unterminated".to_string()]).unwrap_err();
        assert!(matches!(err, WalkError::InvalidExclude { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_with_cycle_detection() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("real/a.wado"));
        // Create a symlink loop: root/loop -> root
        symlink(root, root.join("loop")).unwrap();

        let files = discover_test_files(root, &[]).unwrap().files;
        let got = names_of(root, &files);
        assert!(got.contains("real/a.wado"));
        // The loop must not produce duplicate entries; each canonical path is
        // visited once.
        assert_eq!(files.len(), 1);
    }
}
