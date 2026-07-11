//! `wado clean` — evict derived cache state from the Wado root.
//!
//! Git worktrees are reproducible from a lock's `resolved-ref`, so they are
//! disposable: this removes every `{owner}/{repo}/.worktrees/` directory and
//! prunes the owning clone's admin entries. The canonical clones (shared object
//! stores) and fetched registry components stay unless `--all` is given, so a
//! re-materialize needs no network.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::args::{self, CliExit};

#[derive(Debug, Default)]
pub struct CleanOptions {
    /// Also remove canonical clones and fetched registry components.
    all: bool,
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado clean [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Evict derived cache state (git worktrees) from the Wado root."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(
        buf,
        "      --all   Also remove canonical clones and registry components"
    )
    .unwrap();
    writeln!(buf, "  -h, --help  Show this help message").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<CleanOptions, CliExit> {
    let usage = format_usage();
    let mut opts = CleanOptions::default();
    while let Some(arg) = args::next_arg(&mut parser)? {
        match arg {
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => {
                return Err(CliExit::help(usage));
            }
            lexopt::Arg::Long("all") => opts.all = true,
            other => return Err(args::unexpected_arg(other, &usage)),
        }
    }
    Ok(opts)
}

pub fn run(opts: CleanOptions) -> Result<(), CliExit> {
    let root = crate::cache::root().map_err(CliExit::error)?;
    if !root.is_dir() {
        eprintln!("Nothing to clean ({} does not exist)", root.display());
        return Ok(());
    }

    if opts.all {
        std::fs::remove_dir_all(&root)
            .map_err(|e| CliExit::error(format!("removing {}: {e}", root.display())))?;
        eprintln!("Removed the entire Wado root {}", root.display());
        return Ok(());
    }

    let worktree_dirs = find_worktree_dirs(&root);
    let mut removed = 0;
    for dir in &worktree_dirs {
        // The canonical clone is the worktrees dir's parent; prune its admin
        // entries after removing the checkouts so no dangling refs remain.
        std::fs::remove_dir_all(dir)
            .map_err(|e| CliExit::error(format!("removing {}: {e}", dir.display())))?;
        if let Some(repo) = dir.parent() {
            let _ = crate::git::prune_worktrees(repo);
        }
        removed += 1;
    }
    eprintln!("Removed {removed} worktree cache director(ies)");
    Ok(())
}

/// Every `.worktrees` directory anywhere under `root`.
fn find_worktree_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_worktree_dirs(root, &mut out);
    out
}

fn collect_worktree_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|n| n == ".worktrees") {
            out.push(path);
        } else if path.file_name().is_some_and(|n| n != ".git") {
            // Don't descend into a repo's `.git`; every other dir may hold a
            // nested repo with its own `.worktrees`.
            collect_worktree_dirs(&path, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_worktree_dirs;

    #[test]
    fn finds_nested_worktree_dirs_and_skips_git() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join("github.com/user/router");
        std::fs::create_dir_all(repo.join(".worktrees/1.0.0-abcd1234")).unwrap();
        std::fs::create_dir_all(repo.join(".git/worktrees/x")).unwrap();
        std::fs::create_dir_all(root.join("ghcr.io/ns/pkg/0.1.0")).unwrap();

        let found = find_worktree_dirs(root);
        assert_eq!(found, vec![repo.join(".worktrees")]);
    }
}
