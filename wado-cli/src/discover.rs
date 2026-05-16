//! Test file discovery walker for `wado test`.
//!
//! Walks a project root for `*.wado` files, honouring:
//!
//! - `.gitignore` files at any depth (parsed in-process; no `git` binary required)
//! - submodule directories listed in the root `.gitmodules`
//! - dot-prefixed files and directories
//! - subtrees rooted at a nested `wado.toml` (separate package boundaries)
//! - `[test].exclude` glob patterns from the root manifest
//! - symbolic links (followed once each, with cycle detection on canonical paths)

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use wado_compiler::hashmap::IndexSet;

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

/// Compiled set of `[test].exclude` / `--exclude` glob patterns.
///
/// Owns the [`Pattern`]s once and exposes a single matching entry point so
/// the walker, the discovery driver, and any explicit-arg filtering all
/// interpret patterns identically — including the `dir/**` augmentation
/// (`glob`'s `**` requires at least one trailing path component, so `dir/**`
/// alone would not stop the walker from descending into `dir`).
#[derive(Debug, Default)]
pub struct ExcludeSet {
    patterns: Vec<Pattern>,
}

impl ExcludeSet {
    /// Compile every glob in `patterns`. Errors carry the user-supplied
    /// source string so the CLI can surface "invalid exclude pattern …".
    pub fn compile<S: AsRef<str>>(patterns: &[S]) -> Result<Self, WalkError> {
        let compiled = patterns
            .iter()
            .flat_map(|raw| augmented_patterns(raw.as_ref()))
            .map(|(pattern, source)| {
                Pattern::new(&pattern).map_err(|e| WalkError::InvalidExclude {
                    pattern: source,
                    source: e,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { patterns: compiled })
    }

    /// True when `path` (relative to the matching root) matches any pattern.
    pub fn matches(&self, path: &Path) -> bool {
        self.patterns
            .iter()
            .any(|p| p.matches_path_with(path, WALK_MATCH_OPTIONS))
    }

    /// Convenience for already-rendered path strings.
    pub fn matches_str(&self, path: &str) -> bool {
        self.matches(Path::new(path))
    }

    /// True when no patterns were compiled. Used by the walker to decide
    /// whether to honour `[test].exclude` as a directory-pruning shortcut
    /// (no includes ⇒ exclude is authoritative) or as a file-level filter
    /// only (some includes ⇒ excluded directories may still contain
    /// includable test files and must be descended).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Compiled set of `[test].include` glob patterns: positive overrides that
/// keep files visible to `wado test` even when they match `[test].exclude`.
/// Sharing the same compile pipeline as [`ExcludeSet`] keeps the matcher
/// semantics identical between the two layers.
#[derive(Debug, Default)]
pub struct IncludeSet {
    patterns: Vec<Pattern>,
}

impl IncludeSet {
    pub fn compile<S: AsRef<str>>(patterns: &[S]) -> Result<Self, WalkError> {
        let compiled = patterns
            .iter()
            .flat_map(|raw| augmented_patterns(raw.as_ref()))
            .map(|(pattern, source)| {
                Pattern::new(&pattern).map_err(|e| WalkError::InvalidInclude {
                    pattern: source,
                    source: e,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { patterns: compiled })
    }

    pub fn matches(&self, path: &Path) -> bool {
        self.patterns
            .iter()
            .any(|p| p.matches_path_with(path, WALK_MATCH_OPTIONS))
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Yield the patterns we actually compile for one user-supplied glob.
/// `dir/**` becomes `[dir/**, dir]` so the walker also drops the bare `dir`
/// entry; everything else round-trips unchanged.
fn augmented_patterns(raw: &str) -> impl Iterator<Item = (String, String)> + '_ {
    let prefix = raw
        .strip_suffix("/**")
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let source = raw.to_string();
    std::iter::once((raw.to_string(), source.clone()))
        .chain(prefix.into_iter().map(move |p| (p, source.clone())))
}

#[derive(Debug)]
pub enum WalkError {
    /// I/O failure on a specific filesystem entry (broken symlink,
    /// permission denied, race with deletion, ...). The path is captured
    /// so the runner can report exactly which entry failed.
    Io { path: PathBuf, source: io::Error },
    InvalidExclude {
        pattern: String,
        source: PatternError,
    },
    InvalidInclude {
        pattern: String,
        source: PatternError,
    },
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Io { path, source } => {
                write!(f, "io error reading {}: {source}", path.display())
            }
            WalkError::InvalidExclude { pattern, source } => {
                write!(f, "invalid exclude pattern {pattern:?}: {source}")
            }
            WalkError::InvalidInclude { pattern, source } => {
                write!(f, "invalid include pattern {pattern:?}: {source}")
            }
        }
    }
}

impl std::error::Error for WalkError {}

/// Result of a package-rooted discovery walk.
#[derive(Debug, Default)]
pub struct DiscoveryResult {
    /// `*.wado` files belonging to this package, sorted.
    pub files: Vec<PathBuf>,
    /// Sub-directories that were skipped because they contain their own
    /// `wado.toml`. The caller is expected to recurse into them as separate
    /// package contexts (cargo workspace style).
    pub subpackages: Vec<PathBuf>,
}

/// Discover all `*.wado` files reachable from `root`.
///
/// `excludes` are shell-style globs evaluated against paths relative to `root`.
/// Returned paths are absolute (relative to whatever `root` was given) and
/// sorted for stable output. Returned sub-package roots are sorted as well.
pub fn discover_test_files(
    root: &Path,
    excludes: &ExcludeSet,
    includes: &IncludeSet,
) -> Result<DiscoveryResult, WalkError> {
    let submodules = read_submodule_paths(root)?
        .into_iter()
        .map(|rel| root.join(rel))
        .collect::<IndexSet<_>>();

    let mut visited: IndexSet<PathBuf> = IndexSet::default();
    if let Ok(canon) = fs::canonicalize(root) {
        visited.insert(canon);
    }

    let mut result = DiscoveryResult::default();
    let mut rules: Vec<GitignoreRule> = Vec::new();
    walk_dir(
        root,
        root,
        excludes,
        includes,
        &submodules,
        &mut rules,
        &mut visited,
        &mut result,
    )?;

    result.files.sort();
    result.subpackages.sort();
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    root: &Path,
    excludes: &ExcludeSet,
    includes: &IncludeSet,
    submodules: &IndexSet<PathBuf>,
    rules: &mut Vec<GitignoreRule>,
    visited: &mut IndexSet<PathBuf>,
    out: &mut DiscoveryResult,
) -> Result<(), WalkError> {
    let added_rules = load_gitignore(dir, rules)?;

    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|source| WalkError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        let metadata = fs::metadata(&path).map_err(|source| WalkError::Io {
            path: path.clone(),
            source,
        })?;
        let is_dir = metadata.is_dir();

        if is_dir && submodules.contains(&path) {
            continue;
        }

        if is_ignored(rules, &path, is_dir) {
            continue;
        }

        // Apply `[test].exclude`. With no `[test].include` patterns this is
        // straightforward: a matching path (file or directory) is pruned.
        // When `include` patterns are present they "carve out" excluded
        // regions, so excluded directories must still be descended (any
        // depth, since includes use globs); only files matched by exclude
        // *without* also matching include are dropped.
        //
        // `excluded_descend` records that we entered an excluded directory
        // only because the include set might match something inside it.
        // In that state we still walk for include-matching files, but we
        // do **not** record the directory as a sub-package boundary: the
        // user's `exclude` overrides cross-package recursion regardless of
        // what include patterns this package declares.
        let mut excluded_descend = false;
        if let Ok(rel) = path.strip_prefix(root)
            && excludes.matches(rel)
            && !includes.matches(rel)
        {
            if is_dir && !includes.is_empty() {
                excluded_descend = true;
            } else {
                continue;
            }
        }

        if is_dir {
            // Nested wado.toml: separate package boundary; record it for the
            // caller to recurse into and do not enter it ourselves.
            //
            // Skip the boundary check when we're only descending under
            // `excluded_descend`: the parent excluded this subtree, so the
            // sub-package shouldn't be queued for its own `wado test` run.
            if !excluded_descend && path.join("wado.toml").is_file() {
                out.subpackages.push(path);
                continue;
            }

            if let Ok(canon) = fs::canonicalize(&path)
                && !visited.insert(canon)
            {
                continue;
            }

            walk_dir(
                &path, root, excludes, includes, submodules, rules, visited, out,
            )?;
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
        Err(source) => return Err(WalkError::Io { path, source }),
    };
    Ok(parse_gitmodules(&content))
}

fn parse_gitmodules(content: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in content.lines() {
        // Match `path = value` (leading whitespace permitted). Require a
        // word boundary after the `path` key so we don't pick up siblings
        // like `pathology = …`.
        let Some(rest) = line.trim_start().strip_prefix("path") else {
            continue;
        };
        if !rest
            .chars()
            .next()
            .is_some_and(|c| c == '=' || c.is_whitespace())
        {
            continue;
        }
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        paths.push(PathBuf::from(value.trim()));
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
/// Missing `.gitignore` files are non-fatal; other I/O failures (permission
/// denied, broken symlink, ...) are propagated so the user is told instead
/// of silently losing ignore coverage.
fn load_gitignore(dir: &Path, rules: &mut Vec<GitignoreRule>) -> Result<usize, WalkError> {
    let path = dir.join(".gitignore");
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(WalkError::Io { path, source }),
    };
    let added = parse_gitignore(&content, dir);
    let n = added.len();
    rules.extend(added);
    Ok(n)
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
        let got = names_of(root, &files);
        assert!(got.contains("a.wado"));
        assert!(got.contains("nested/a.wado"));
        assert!(!got.contains("nested/skipme.wado"));
    }

    #[test]
    fn gitmodules_parser_requires_word_boundary_after_path_key() {
        let content = "\
[submodule \"sub\"]\n\
\tpath = vendor/sub\n\
\turl  = https://example.com\n\
[submodule \"siblings\"]\n\
pathology = should-not-match\n\
\tpath=vendor/no-spaces\n\
";
        let paths: Vec<String> = parse_gitmodules(content)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["vendor/sub", "vendor/no-spaces"]);
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
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

        let result =
            discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default()).unwrap();
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

        let files = discover_test_files(
            root,
            &ExcludeSet::compile(&["compiler/tests/**".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
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

        let result = discover_test_files(
            root,
            &ExcludeSet::compile(&["skip/**".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
        .unwrap();
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

        let result = discover_test_files(
            root,
            &ExcludeSet::compile(&["subpkg/**".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
        .unwrap();
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

        let files = discover_test_files(
            root,
            &ExcludeSet::compile(&["src/*.wado".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
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

        let files = discover_test_files(
            root,
            &ExcludeSet::compile(&["**/foo.wado".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
        .unwrap()
        .files;
        let got = names_of(root, &files);
        assert!(!got.iter().any(|p| p.ends_with("/foo.wado")));
        assert!(got.contains("a/b/keep.wado"));
    }

    #[test]
    fn rejects_invalid_exclude_pattern() {
        let err = ExcludeSet::compile(&["[unterminated".to_string()]).unwrap_err();
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

        let files = discover_test_files(root, &ExcludeSet::default(), &IncludeSet::default())
            .unwrap()
            .files;
        let got = names_of(root, &files);
        assert!(got.contains("real/a.wado"));
        // The loop must not produce duplicate entries; each canonical path is
        // visited once.
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn include_carves_out_excluded_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("lib/core/prelude/string.wado"));
        touch(&root.join("lib/core/prelude/string_test.wado"));
        touch(&root.join("lib/core/prelude/nested/array.wado"));
        touch(&root.join("lib/core/prelude/nested/array_test.wado"));
        touch(&root.join("lib/core/cli.wado"));

        let files = discover_test_files(
            root,
            &ExcludeSet::compile(&["lib/core/prelude/**".to_string()]).unwrap(),
            &IncludeSet::compile(&["lib/**/*_test.wado".to_string()]).unwrap(),
        )
        .unwrap()
        .files;
        let got = names_of(root, &files);
        // Non-test files in excluded subtree are still pruned.
        assert!(!got.contains("lib/core/prelude/string.wado"));
        assert!(!got.contains("lib/core/prelude/nested/array.wado"));
        // *_test.wado in excluded subtree (at any depth) is discovered.
        assert!(got.contains("lib/core/prelude/string_test.wado"));
        assert!(got.contains("lib/core/prelude/nested/array_test.wado"));
        // Files outside the excluded subtree are unaffected.
        assert!(got.contains("lib/core/cli.wado"));
    }

    #[test]
    fn empty_include_keeps_exclude_pruning_behaviour() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.wado"));
        touch(&root.join("excluded/b.wado"));
        touch(&root.join("excluded/b_test.wado"));

        let files = discover_test_files(
            root,
            &ExcludeSet::compile(&["excluded/**".to_string()]).unwrap(),
            &IncludeSet::default(),
        )
        .unwrap()
        .files;
        let got = names_of(root, &files);
        assert!(got.contains("a.wado"));
        // Without includes, even `*_test.wado` under an excluded dir is pruned.
        assert!(!got.contains("excluded/b.wado"));
        assert!(!got.contains("excluded/b_test.wado"));
    }

    #[test]
    fn excluded_subpackage_not_recorded_even_with_includes() {
        // Regression: when `[test].include` is non-empty, the walker
        // descends excluded directories to look for include matches. It
        // must not, however, treat a nested `wado.toml` inside an excluded
        // subtree as a sub-package — the user's exclude should still
        // suppress cross-package recursion.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("vendor/wado.toml"));
        touch(&root.join("vendor/src/a.wado"));
        touch(&root.join("lib/stdlib_test.wado"));

        let result = discover_test_files(
            root,
            &ExcludeSet::compile(&["vendor/**".to_string()]).unwrap(),
            &IncludeSet::compile(&["lib/**/*_test.wado".to_string()]).unwrap(),
        )
        .unwrap();
        let got = names_of(root, &result.files);
        // The include-matched test in `lib/` is still discovered.
        assert!(got.contains("lib/stdlib_test.wado"));
        // The excluded sub-package directory is not queued for recursion.
        assert!(result.subpackages.is_empty(), "{:?}", result.subpackages);
        // And nothing inside it was discovered.
        assert!(!got.contains("vendor/src/a.wado"));
    }

    #[test]
    fn invalid_include_pattern_says_include() {
        // Regression: `IncludeSet::compile` used to report a bad glob via
        // `WalkError::InvalidExclude`, producing a misleading "invalid
        // exclude pattern" message for what is really an `[test].include`
        // problem.
        let err = IncludeSet::compile(&["[unterminated".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid include pattern"), "{msg}");
        assert!(!msg.contains("invalid exclude pattern"), "{msg}");
    }
}
