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

    // run: with no prior `wado fetch`, the build path auto-materializes the
    // locked worktree, compiles the git-sourced library into the consumer, and
    // executes it.
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from git dep"));

    let worktrees = cache
        .join(wado_manifest::cache::git_repo_relative(&url).unwrap())
        .join(".worktrees");
    assert!(
        worktrees.is_dir(),
        "run should have materialized a worktree under {}",
        worktrees.display()
    );

    // fetch is still available and idempotent against the warm cache.
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .arg("fetch")
        .assert()
        .success();
}

/// A monorepo git dependency: one repository holds several packages in
/// subdirectories, and `directory` selects which one. `update` reads the
/// subdirectory's `wado.toml`, `run` compiles that package's library into the
/// consumer.
#[test]
fn git_dependency_with_directory_selects_a_monorepo_subpackage() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache = root.join("wado-cache");

    // A monorepo with two packages; only `packages/foo` is depended on.
    let repo = root.join("monorepo");
    for (dir, name, ret) in [
        ("packages/foo", "foo", "from foo"),
        ("packages/bar", "bar", "from bar"),
    ] {
        fs::create_dir_all(repo.join(dir).join("src")).unwrap();
        fs::write(
            repo.join(dir).join("wado.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n"),
        )
        .unwrap();
        fs::write(
            repo.join(dir).join("src/lib.wado"),
            format!("export fn hello() -> String {{\n    return \"{ret}\";\n}}\n"),
        )
        .unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "monorepo"]);
    let url = format!("file://{}", repo.display());

    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n\n\
             [dependencies]\n\
             \"org:foo\" = {{ git = \"{url}\", ref = \"main\", directory = \"packages/foo\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        "use { println, Stdout } from \"core:cli\";\n\
         use { hello } from \"org:foo\";\n\n\
         export fn run() with Stdout {\n    println(hello());\n}\n",
    )
    .unwrap();

    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .arg("update")
        .assert()
        .success();

    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from foo"))
        .stdout(predicate::str::contains("from bar").not());
}

/// Submodules are populated by default: a git dependency whose repository has a
/// submodule gets that submodule checked out into the materialized worktree, so
/// the dependency's full source is present.
#[test]
fn git_dependency_materializes_submodules_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache = root.join("wado-cache");

    // A throwaway global git config allowing local (`file`) submodule transport,
    // which git blocks by default. Scoped to this test's processes via
    // GIT_CONFIG_GLOBAL, so the production default stays secure.
    let gitconfig = root.join("gitconfig");
    fs::write(&gitconfig, "[protocol \"file\"]\n\tallow = always\n").unwrap();
    let cfg = |cmd: &mut Command| {
        cmd.env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
    };
    let git_cfg = |dir: &Path, args: &[&str]| {
        let mut c = Command::new("git");
        c.current_dir(dir).args(args);
        cfg(&mut c);
        assert!(c.status().unwrap().success(), "git {args:?} failed");
    };

    // The submodule repository carries a marker file.
    let sub = root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git_cfg(&sub, &["init", "-q", "-b", "main"]);
    fs::write(sub.join("token.txt"), "SUBTOKEN\n").unwrap();
    git_cfg(&sub, &["add", "-A"]);
    git_cfg(&sub, &["commit", "-q", "-m", "sub"]);

    // The dependency repository embeds it as a submodule.
    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    git_cfg(&greet, &["init", "-q", "-b", "main"]);
    fs::write(
        greet.join("wado.toml"),
        "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(greet.join("src/lib.wado"), "export fn hello() -> String { return \"hi\"; }\n")
        .unwrap();
    git_cfg(
        &greet,
        &["submodule", "add", &format!("file://{}", sub.display()), "vendor/sub"],
    );
    git_cfg(&greet, &["add", "-A"]);
    git_cfg(&greet, &["commit", "-q", "-m", "greet with submodule"]);
    let url = format!("file://{}", greet.display());

    let app = root.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("wado.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\ngreet = {{ git = \"{url}\", ref = \"main\" }}\n"
        ),
    )
    .unwrap();

    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .arg("update")
        .assert()
        .success();
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .arg("fetch")
        .assert()
        .success();

    // The materialized worktree must contain the checked-out submodule file.
    let worktrees = cache
        .join(wado_manifest::cache::git_repo_relative(&url).unwrap())
        .join(".worktrees");
    let entry = fs::read_dir(&worktrees)
        .unwrap()
        .next()
        .expect("a worktree exists")
        .unwrap()
        .path();
    let token = entry.join("vendor/sub/token.txt");
    assert!(
        token.is_file(),
        "submodule file should be checked out at {}",
        token.display()
    );
}

/// A single-file script (no `wado.toml`) pins a git source inline on the `use`
/// clause. `wado run <file>` resolves the ref, materializes the worktree, and
/// compiles the git-sourced library into the script.
#[test]
fn inline_git_source_in_a_single_file_script() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache = root.join("wado-cache");

    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        "export fn hello() -> String {\n    return \"hello from inline git\";\n}\n",
    )
    .unwrap();
    git(&greet, &["init", "-q", "-b", "main"]);
    git(&greet, &["add", "-A"]);
    git(&greet, &["commit", "-q", "-m", "init"]);
    let url = format!("file://{}", greet.display());

    // A bare script with no manifest, pinning the git source inline.
    let script_dir = root.join("script");
    fs::create_dir_all(&script_dir).unwrap();
    fs::write(
        script_dir.join("main.wado"),
        format!(
            "use {{ println, Stdout }} from \"core:cli\";\n\
             use {{ hello }} from \"greet\" with {{ git: \"{url}\", ref: \"main\" }};\n\n\
             export fn run() with Stdout {{\n    println(hello());\n}}\n"
        ),
    )
    .unwrap();

    wado_in(&script_dir)
        .env("WADO_ROOT", &cache)
        .args(["run", "main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from inline git"));
}
