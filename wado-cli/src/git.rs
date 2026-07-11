//! System `git` backend for git dependencies: tag/ref discovery, a checkout-free
//! manifest read, and per-version worktree materialization under the Wado root.
//!
//! All mutations of one repository (clone, fetch, `worktree add`) are serialized
//! by a per-repo advisory file lock, so concurrent `wado` processes (parallel
//! builds, the LSP alongside a CLI run) never race a half-written tree. Reads of
//! an already-materialized worktree take no lock — a completed worktree is
//! immutable. Acquisition is two-tier: resolution reads a manifest from a blob
//! (`git show <sha>:…`) with no checkout; materialization adds a worktree only
//! when source files must be on disk.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use wado_manifest::{GitTagInfo, Manifest, ProviderError};

use crate::registry::parse_version_tag;

/// List a repository's semver tags via `git ls-remote --tags`.
pub fn list_tags(url: &str) -> Result<Vec<GitTagInfo>, ProviderError> {
    let out = run_git(None, &["ls-remote", "--tags", url])?;
    // Map tag name → sha, preferring an annotated tag's peeled (`^{}`) commit
    // over the tag object it points through.
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    for line in out.lines() {
        let Some((sha, git_ref)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = git_ref.strip_prefix("refs/tags/") else {
            continue;
        };
        let (name, peeled) = match tag.strip_suffix("^{}") {
            Some(name) => (name, true),
            None => (tag, false),
        };
        if peeled || !by_name.contains_key(name) {
            by_name.insert(name.to_string(), sha.to_string());
        }
    }
    Ok(by_name
        .into_iter()
        .filter_map(|(name, sha)| parse_version_tag(&name).map(|version| GitTagInfo { version, sha }))
        .collect())
}

/// Resolve a named ref (branch/tag) to a full commit SHA via `git ls-remote`. A
/// ref that is itself a commit SHA resolves to itself.
pub fn resolve_ref(url: &str, git_ref: &str) -> Result<String, ProviderError> {
    let out = run_git(None, &["ls-remote", url, git_ref])?;
    if let Some((sha, _)) = out.lines().next().and_then(|l| l.split_once('\t')) {
        return Ok(sha.to_string());
    }
    if is_hex_sha(git_ref) {
        return Ok(git_ref.to_string());
    }
    Err(ProviderError::NotFound {
        source: format!("{url}#{git_ref}"),
        message: "git ref not found".to_string(),
    })
}

/// Read a git package's manifest at `sha` without a checkout: ensure the clone
/// and the commit's objects, then `git show <sha>:<directory>/wado.toml`.
pub fn fetch_manifest(
    url: &str,
    sha: &str,
    directory: Option<&str>,
) -> Result<Manifest, ProviderError> {
    let repo_rel = repo_relative(url)?;
    let repo = wado_root()?.join(&repo_rel);
    let _lock = lock_repo(&repo_rel)?;
    ensure_repo(url, &repo)?;
    ensure_commit(url, &repo, sha)?;
    let path = match directory {
        Some(dir) => format!("{sha}:{}/wado.toml", dir.trim_matches('/')),
        None => format!("{sha}:wado.toml"),
    };
    let text = run_git(Some(&repo), &["show", &path])?;
    text.parse::<Manifest>()
        .map_err(|e| ProviderError::InvalidManifest {
            source: format!("{url}@{sha}"),
            message: e.to_string(),
        })
}

/// Materialize a per-version worktree at `sha` under the Wado root, returning its
/// absolute path. Idempotent: a valid warm worktree is reused; a stale/partial
/// one is rebuilt.
pub fn materialize(url: &str, version: &str, sha: &str) -> Result<PathBuf, ProviderError> {
    let repo_rel = repo_relative(url)?;
    let worktree_rel = wado_manifest::cache::git_worktree_relative(url, version, sha)
        .ok_or_else(|| bad_url(url))?;
    let root = wado_root()?;
    let worktree = root.join(&worktree_rel);
    if worktree_is_valid(&worktree, sha) {
        return Ok(worktree);
    }
    let repo = root.join(&repo_rel);
    let _lock = lock_repo(&repo_rel)?;
    ensure_repo(url, &repo)?;
    ensure_commit(url, &repo, sha)?;
    // A crash can leave a partial worktree or a dangling admin entry; prune and
    // force-remove before re-adding so the add never trips over leftovers.
    let _ = run_git(Some(&repo), &["worktree", "prune"]);
    if worktree.exists() {
        let _ = run_git(
            Some(&repo),
            &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        );
    }
    run_git(
        Some(&repo),
        &["worktree", "add", "--detach", &worktree.to_string_lossy(), sha],
    )?;
    if !worktree_is_valid(&worktree, sha) {
        return Err(ProviderError::IoError {
            path: worktree.display().to_string(),
            message: format!("worktree did not check out to {sha}"),
        });
    }
    // Populate submodules by default (safe side): a dependency's submodules are
    // part of its source, so a checkout that omitted them would miss code the
    // library needs. A no-op for a repo without submodules. Only on (re)create,
    // not the warm-hit path above — a worktree this code built already has them.
    run_git(
        Some(&worktree),
        &["submodule", "update", "--init", "--recursive"],
    )?;
    Ok(worktree)
}

/// Clone the canonical repository if it is not already present. The clone is a
/// normal (non-bare) checkout so it stays ghq-browsable; `.worktrees/` is added
/// to `.git/info/exclude` so nested worktrees never show as untracked.
fn ensure_repo(url: &str, repo: &Path) -> Result<(), ProviderError> {
    if repo.join(".git").exists() {
        return Ok(());
    }
    if let Some(parent) = repo.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
    }
    run_git(None, &["clone", url, &repo.to_string_lossy()])?;
    let exclude = repo.join(".git").join("info").join("exclude");
    if let Ok(text) = fs::read_to_string(&exclude)
        && !text.lines().any(|l| l.trim() == ".worktrees/")
    {
        let _ = fs::write(&exclude, format!("{text}\n.worktrees/\n"));
    }
    Ok(())
}

/// Ensure the commit's objects are present, fetching if needed. Prefers a narrow
/// by-SHA fetch, falling back to a full branch+tag fetch when the server rejects
/// it (`uploadpack.allowReachableSHA1InWant` off).
fn ensure_commit(url: &str, repo: &Path, sha: &str) -> Result<(), ProviderError> {
    if commit_present(repo, sha) {
        return Ok(());
    }
    if run_git(Some(repo), &["fetch", "--depth", "1", "origin", sha]).is_ok()
        && commit_present(repo, sha)
    {
        return Ok(());
    }
    run_git(Some(repo), &["fetch", "--tags", "--force", "origin"])?;
    if commit_present(repo, sha) {
        return Ok(());
    }
    Err(ProviderError::NotFound {
        source: format!("{url}@{sha}"),
        message: "commit not found in the repository after fetch".to_string(),
    })
}

/// Materialize a worktree and return its absolute `[package].lib` entry path —
/// the shared spine both the manifest build-tier materializer and the
/// single-file inline resolver end at, so a git dependency's source entry is
/// located identically however it was declared. `directory` selects a monorepo
/// subdirectory within the worktree.
pub fn materialize_entry(
    url: &str,
    version: &str,
    sha: &str,
    directory: Option<&str>,
) -> Result<String, ProviderError> {
    let worktree = materialize(url, version, sha)?;
    let pkg_dir = match directory {
        Some(dir) => worktree.join(dir),
        None => worktree,
    };
    let entry = wado_lsp::host::package_lib_entry(&pkg_dir).map_err(|message| {
        ProviderError::InvalidManifest {
            source: pkg_dir.display().to_string(),
            message,
        }
    })?;
    Ok(entry.display().to_string())
}

/// Prune a clone's worktree admin entries for checkouts removed on disk. Used by
/// `wado clean` after deleting the `.worktrees/` directory. Best-effort.
pub fn prune_worktrees(repo: &Path) -> Result<(), ProviderError> {
    run_git(Some(repo), &["worktree", "prune"]).map(|_| ())
}

fn commit_present(repo: &Path, sha: &str) -> bool {
    run_git(Some(repo), &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

fn worktree_is_valid(worktree: &Path, sha: &str) -> bool {
    if !worktree.join("wado.toml").exists() && !worktree.exists() {
        return false;
    }
    run_git(Some(worktree), &["rev-parse", "HEAD"])
        .map(|head| head.trim().starts_with(sha))
        .unwrap_or(false)
}

/// Run `git`, returning stdout on success. A missing `git` binary or a non-zero
/// exit becomes a descriptive [`ProviderError`].
fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, ProviderError> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ProviderError::NotFound {
                source: "git".to_string(),
                message: "the `git` executable was not found on PATH".to_string(),
            }
        } else {
            ProviderError::NetworkError {
                url: format!("git {}", args.join(" ")),
                message: e.to_string(),
            }
        }
    })?;
    if !output.status.success() {
        // stderr is only a diagnostic, so a lossy decode is acceptable here.
        return Err(ProviderError::NetworkError {
            url: format!("git {}", args.join(" ")),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    // stdout is parsed as data (ref lists, a manifest blob, a SHA), and stdio is
    // not guaranteed UTF-8 in general, so reject invalid bytes rather than
    // silently replacing them and parsing garbage.
    String::from_utf8(output.stdout).map_err(|e| ProviderError::InvalidManifest {
        source: format!("git {}", args.join(" ")),
        message: format!("output is not valid UTF-8: {e}"),
    })
}

fn repo_relative(url: &str) -> Result<String, ProviderError> {
    wado_manifest::cache::git_repo_relative(url).ok_or_else(|| bad_url(url))
}

fn wado_root() -> Result<PathBuf, ProviderError> {
    crate::cache::root().map_err(|message| ProviderError::IoError {
        path: "<wado-root>".to_string(),
        message,
    })
}

fn bad_url(url: &str) -> ProviderError {
    ProviderError::NotFound {
        source: url.to_string(),
        message: "not a host/owner/repo git URL".to_string(),
    }
}

fn io_err(path: &Path, e: &std::io::Error) -> ProviderError {
    ProviderError::IoError {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

fn is_hex_sha(s: &str) -> bool {
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// An exclusive advisory lock over one repository's mutations, held for the
/// guard's lifetime. On unix it is a `flock` on a per-repo lock file under
/// `<root>/.locks/`; elsewhere it degrades to a no-op (best effort).
struct RepoLock {
    #[cfg(unix)]
    _file: fs::File,
}

fn lock_repo(repo_rel: &str) -> Result<RepoLock, ProviderError> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let locks = wado_root()?.join(".locks");
        fs::create_dir_all(&locks).map_err(|e| io_err(&locks, &e))?;
        let path = locks.join(format!("{}.lock", repo_rel.replace('/', "_")));
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io_err(&path, &e))?;
        // SAFETY: `file` owns the fd for the duration of the flock call; the lock
        // is released when the returned guard (and its `File`) is dropped.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io_err(&path, &std::io::Error::last_os_error()));
        }
        Ok(RepoLock { _file: file })
    }
    #[cfg(not(unix))]
    {
        let _ = repo_rel;
        Ok(RepoLock {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wado_manifest::Version;

    fn init_repo(dir: &Path) {
        run_git(Some(dir), &["init", "-q", "-b", "main"]).unwrap();
        run_git(Some(dir), &["config", "user.email", "t@t"]).unwrap();
        run_git(Some(dir), &["config", "user.name", "t"]).unwrap();
    }

    fn commit_all(dir: &Path, message: &str) -> String {
        run_git(Some(dir), &["add", "-A"]).unwrap();
        run_git(Some(dir), &["commit", "-q", "-m", message]).unwrap();
        run_git(Some(dir), &["rev-parse", "HEAD"]).unwrap().trim().to_string()
    }

    // A local `file://` origin exercises the real git plumbing with no network.
    fn origin_url(dir: &Path) -> String {
        format!("file://{}", dir.display())
    }

    #[test]
    fn lists_semver_tags_and_resolves_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("router");
        fs::create_dir(&origin).unwrap();
        init_repo(&origin);
        fs::write(origin.join("wado.toml"), "[package]\nname=\"router\"\nversion=\"1.0.0\"\n")
            .unwrap();
        let sha_10 = commit_all(&origin, "v1.0.0");
        run_git(Some(&origin), &["tag", "v1.0.0"]).unwrap();
        fs::write(origin.join("x.txt"), "x").unwrap();
        commit_all(&origin, "wip");
        run_git(Some(&origin), &["tag", "not-semver"]).unwrap();

        let url = origin_url(&origin);
        let tags = list_tags(&url).unwrap();
        assert_eq!(tags.len(), 1, "only the semver tag is kept: {tags:?}");
        assert_eq!(tags[0].version, Version::parse("1.0.0").unwrap());
        assert_eq!(tags[0].sha, sha_10);

        assert_eq!(resolve_ref(&url, "main").unwrap().len(), 40);
        assert_eq!(resolve_ref(&url, "v1.0.0").unwrap(), sha_10);
    }

    #[test]
    fn fetches_manifest_and_materializes_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wado-root");
        let origin = tmp.path().join("lib");
        fs::create_dir(&origin).unwrap();
        init_repo(&origin);
        fs::write(
            origin.join("wado.toml"),
            "[package]\nname=\"lib\"\nversion=\"0.3.0\"\nlib=\"src/lib.wado\"\n",
        )
        .unwrap();
        fs::create_dir(origin.join("src")).unwrap();
        fs::write(origin.join("src/lib.wado"), "export fn f() -> i32 { return 1 }\n").unwrap();
        let sha = commit_all(&origin, "init");
        let url = origin_url(&origin);

        // SAFETY: single-threaded test; WADO_ROOT is process-global here.
        unsafe { std::env::set_var("WADO_ROOT", &root) };

        let manifest = fetch_manifest(&url, &sha, None).unwrap();
        assert_eq!(manifest.package.unwrap().version, "0.3.0");

        let worktree = materialize(&url, "0.3.0", &sha).unwrap();
        assert!(worktree.join("src/lib.wado").is_file());
        assert!(
            worktree.ends_with(format!(".worktrees/0.3.0-{}", &sha[..8])),
            "{}",
            worktree.display()
        );
        // A second call is a warm hit (idempotent).
        assert_eq!(materialize(&url, "0.3.0", &sha).unwrap(), worktree);

        unsafe { std::env::remove_var("WADO_ROOT") };
    }
}
