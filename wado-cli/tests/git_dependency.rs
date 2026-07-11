//! End-to-end test for a `git` dependency: a local `file://` repository is a
//! Wado source library that the consumer imports by name. Drives the real
//! `wado` binary through `update` (resolve + lock the commit) → `fetch`
//! (materialize the worktree under a temp Wado root) → `run` (compile the
//! git-sourced library into the consumer and execute), with no network.

use predicates::prelude::*;
use std::fs;
use std::path::Path;
use std::process::Command;

mod common;
use common::wado_in;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn git_dependency_resolves_fetches_and_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache = root.join("wado-cache");

    // A Wado library published as a git repository.
    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        "export fn hello() -> String {\n    return \"hello from git dep\";\n}\n",
    )
    .unwrap();
    git(&greet, &["init", "-q", "-b", "main"]);
    git(&greet, &["add", "-A"]);
    git(&greet, &["commit", "-q", "-m", "init"]);

    let url = format!("file://{}", greet.display());

    // The consumer depends on it by git ref and imports it by name.
    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n\n\
             [dependencies]\ngreet = {{ git = \"{url}\", ref = \"main\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        "use { println, Stdout } from \"core:cli\";\n\
         use { hello } from \"greet\";\n\n\
         export fn run() with Stdout {\n    println(hello());\n}\n",
    )
    .unwrap();

    // update: resolve the ref to a commit and lock it.
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .arg("update")
        .assert()
        .success();
    let lock = fs::read_to_string(app.join("wado.lock")).unwrap();
    assert!(lock.contains("git+"), "lock missing git entry:\n{lock}");
    assert!(
        lock.contains("resolved-ref"),
        "lock missing resolved-ref:\n{lock}"
    );

    // fetch: materialize the worktree into the Wado root.
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .arg("fetch")
        .assert()
        .success();
    let worktrees = cache
        .join(wado_manifest::cache::git_repo_relative(&url).unwrap())
        .join(".worktrees");
    assert!(
        worktrees.is_dir(),
        "expected a materialized worktree under {}",
        worktrees.display()
    );

    // run: compile the git-sourced library into the consumer and execute.
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from git dep"));
}
