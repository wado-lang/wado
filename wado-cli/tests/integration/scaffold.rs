//! `wado init` / `wado new` and `wado workspace init` / `new` / `add`: what
//! each writes, and the `members` entry a package under a workspace gets.

use predicates::prelude::*;
use std::fs;
use std::path::Path;
use wado_manifest::WorkspaceManifest;

use crate::common::wado_in;

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn init_writes_a_library_package_named_after_its_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("geo");
    fs::create_dir(&dir).unwrap();

    wado_in(&dir)
        .args(["init", "--namespace", "acme"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Created wado.toml"));

    let manifest = read(&dir.join("wado.toml"));
    assert!(manifest.contains("namespace = \"acme\""), "{manifest}");
    assert!(manifest.contains("name = \"geo\""), "{manifest}");
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
    assert!(manifest.contains("lib = \"src/lib.wado\""), "{manifest}");
    assert!(!manifest.contains("[world]"), "{manifest}");
    assert!(read(&dir.join("README.md")).starts_with("# geo\n"));
    assert!(read(&dir.join("AGENTS.md")).starts_with("# geo\n"));
    assert!(dir.join("src/lib.wado").is_file());
    assert!(dir.join("tests/geo_test.wado").is_file());
    assert!(!dir.join("src/main.wado").exists());
}

#[test]
fn init_cli_writes_a_command_world() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "tool", "--namespace", "acme", "--cli"])
        .assert()
        .success();

    let manifest = read(&tmp.path().join("wado.toml"));
    assert!(
        manifest.contains("[world]\n\"wasi:cli/command\" = \"src/main.wado\""),
        "{manifest}"
    );
    assert!(!manifest.contains("lib ="), "{manifest}");
    assert!(tmp.path().join("src/main.wado").is_file());
    assert!(!tmp.path().join("src/lib.wado").exists());
    assert!(!tmp.path().join("tests").exists());
}

#[test]
fn init_lib_and_cli_writes_both() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args([
            "init",
            "--name",
            "both",
            "--namespace",
            "acme",
            "--lib",
            "--cli",
        ])
        .assert()
        .success();

    let manifest = read(&tmp.path().join("wado.toml"));
    assert!(manifest.contains("lib = \"src/lib.wado\""), "{manifest}");
    assert!(manifest.contains("\"wasi:cli/command\""), "{manifest}");
    assert!(tmp.path().join("src/lib.wado").is_file());
    assert!(tmp.path().join("src/main.wado").is_file());
}

/// `init` adopts a directory that already holds work, so a file of the
/// scaffold that is already there is kept as it is.
#[test]
fn init_keeps_a_file_already_there() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("README.md"), "mine\n").unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "app", "--namespace", "acme"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Kept README.md"));

    assert_eq!(read(&tmp.path().join("README.md")), "mine\n");
}

#[test]
fn init_requires_namespace_outside_a_workspace() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "my-app"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--namespace is required"));
}

#[test]
fn init_rejects_a_directory_name_that_is_no_package_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("my.app");
    fs::create_dir(&dir).unwrap();

    wado_in(&dir)
        .args(["init", "--namespace", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--name"));
    assert!(!dir.join("wado.toml").exists());
}

#[test]
fn init_fails_when_manifest_exists() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "my-app", "--namespace", "myorg"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--force"));
}

#[test]
fn init_force_overwrites_the_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"old\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args([
            "init",
            "--name",
            "new-app",
            "--namespace",
            "myorg",
            "--force",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Created wado.toml"));

    assert!(read(&tmp.path().join("wado.toml")).contains("name = \"new-app\""));
}

#[test]
fn init_help() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado init"));
}

/// A workspace member takes the fields `[workspace.package]` sets from the
/// workspace, so `init` there writes neither them nor asks for them.
#[test]
fn init_in_a_workspace_member_leaves_out_what_it_inherits() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado-workspace.toml"),
        "[workspace]\nmembers = [\"pkgs/*\"]\n\n\
         [workspace.package]\nnamespace = \"acme\"\nversion = \"0.4.0\"\n",
    )
    .unwrap();
    let member = tmp.path().join("pkgs/app");
    fs::create_dir_all(&member).unwrap();

    wado_in(&member)
        .args(["init", "--namespace", "myorg"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "the workspace sets the namespace `acme`",
        ));
    wado_in(&member).arg("init").assert().success();

    let manifest = read(&member.join("wado.toml"));
    assert!(!manifest.contains("namespace ="), "{manifest}");
    assert!(!manifest.contains("version ="), "{manifest}");
    assert!(
        manifest.contains("# version / namespace inherited from [workspace.package]"),
        "{manifest}"
    );
    assert_eq!(
        read(&tmp.path().join("wado-workspace.toml")),
        "[workspace]\nmembers = [\"pkgs/*\"]\n\n\
         [workspace.package]\nnamespace = \"acme\"\nversion = \"0.4.0\"\n",
        "a member `members` already covers leaves the workspace as it was"
    );
}

#[test]
fn new_creates_the_directory_and_initializes_it() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["new", "nested/geo", "--namespace", "acme"])
        .assert()
        .success();

    let dir = tmp.path().join("nested/geo");
    assert!(read(&dir.join("wado.toml")).contains("name = \"geo\""));
    assert!(dir.join("src/lib.wado").is_file());
}

/// `new` is `mkdir -p <dir> && cd <dir> && wado init`, so a directory that is
/// already there is initialized, not refused.
#[test]
fn new_initializes_a_directory_already_there() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("geo")).unwrap();

    wado_in(tmp.path())
        .args(["new", "geo", "--namespace", "acme", "--name", "geometry"])
        .assert()
        .success();

    assert!(read(&tmp.path().join("geo/wado.toml")).contains("name = \"geometry\""));
}

#[test]
fn new_requires_a_directory() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["new", "--namespace", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: wado new"));
}

/// A package created under a workspace whose `members` misses it becomes a
/// member: its path joins `members`, in order, and the rest of the file stays
/// as written.
#[test]
fn new_under_a_workspace_adds_it_to_members() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::write(
        tmp.path().join("wado-workspace.toml"),
        "# the tools\n[workspace]\nmembers = [\"a\", \"c\"]\n\n\
         [workspace.package]\nnamespace = \"acme\"\n\
         repository = \"https://example.com/acme/tools\"\n",
    )
    .unwrap();

    wado_in(tmp.path()).args(["new", "b"]).assert().success();

    assert_eq!(
        read(&tmp.path().join("wado-workspace.toml")),
        "# the tools\n[workspace]\nmembers = [\n    \"a\",\n    \"b\",\n    \"c\",\n]\n\n\
         [workspace.package]\nnamespace = \"acme\"\n\
         repository = \"https://example.com/acme/tools\"\n",
    );
    let manifest = read(&tmp.path().join("b/wado.toml"));
    assert!(
        manifest.contains("repository-directory = \"b\""),
        "{manifest}"
    );
    assert!(
        manifest.contains("# repository / namespace inherited from [workspace.package]"),
        "{manifest}"
    );
}

/// A directory git ignores is scratch space, not a member: the package made
/// there stands alone and the workspace file stays as written.
#[test]
fn new_in_a_git_ignored_directory_stays_out_of_the_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join(".gitignore"), "scratch/\n").unwrap();
    let ws = "[workspace]\nmembers = []\n\n[workspace.package]\nnamespace = \"acme\"\n";
    fs::write(tmp.path().join("wado-workspace.toml"), ws).unwrap();

    wado_in(tmp.path())
        .args(["new", "scratch/demo", "--namespace", "mine"])
        .assert()
        .success()
        .stderr(predicate::str::contains("ignored by git"));

    assert_eq!(read(&tmp.path().join("wado-workspace.toml")), ws);
    let manifest = read(&tmp.path().join("scratch/demo/wado.toml"));
    assert!(manifest.contains("namespace = \"mine\""), "{manifest}");
}

/// The scaffold is a package that passes its own tests.
#[test]
fn new_package_passes_its_tests() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["new", "geo", "--namespace", "acme", "--lib", "--cli"])
        .assert()
        .success();
    let dir = tmp.path().join("geo");
    wado_in(&dir).arg("test").assert().success();
    wado_in(&dir)
        .args(["run", "src/main.wado"])
        .assert()
        .success();
}

#[test]
fn workspace_init_writes_the_workspace_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tools");
    fs::create_dir(&dir).unwrap();

    wado_in(&dir)
        .args(["workspace", "init", "--namespace", "acme"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Created wado-workspace.toml"));

    let text = read(&dir.join("wado-workspace.toml"));
    assert_eq!(
        text,
        "[workspace]\nmembers = []\n\n[workspace.package]\nnamespace = \"acme\"\n"
    );
    text.parse::<WorkspaceManifest>().unwrap();
    assert!(read(&dir.join("AGENTS.md")).starts_with("# tools\n"));
    assert!(!dir.join("wado.toml").exists());

    wado_in(&dir)
        .args(["workspace", "init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn workspace_init_refuses_a_package_directory() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["workspace", "init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wado.toml"));
    assert!(!tmp.path().join("wado-workspace.toml").exists());
}

#[test]
fn workspace_new_then_new_member() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["workspace", "new", "tools", "--namespace", "acme"])
        .assert()
        .success();
    let root = tmp.path().join("tools");
    wado_in(&root).args(["new", "geo"]).assert().success();

    assert_eq!(
        read(&root.join("wado-workspace.toml")),
        "[workspace]\nmembers = [\n    \"geo\",\n]\n\n[workspace.package]\nnamespace = \"acme\"\n"
    );
    assert!(!read(&root.join("geo/wado.toml")).contains("namespace ="));
}

#[test]
fn workspace_add_adds_an_existing_package() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado-workspace.toml"),
        "[workspace]\nmembers = []\n",
    )
    .unwrap();
    let pkg = tmp.path().join("pkgs/geo");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"geo\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["workspace", "add", "pkgs/geo"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Added pkgs/geo"));
    let joined = "[workspace]\nmembers = [\n    \"pkgs/geo\",\n]\n";
    assert_eq!(read(&tmp.path().join("wado-workspace.toml")), joined);

    wado_in(&pkg)
        .args(["workspace", "add", "."])
        .assert()
        .success()
        .stderr(predicate::str::contains("already a member"));
    assert_eq!(read(&tmp.path().join("wado-workspace.toml")), joined);
}

/// A package that sets what the workspace forces cannot join it, and the
/// refusal leaves the workspace file as it was.
#[test]
fn workspace_add_refuses_a_package_that_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.0.0\"\n";
    fs::write(tmp.path().join("wado-workspace.toml"), ws).unwrap();
    let pkg = tmp.path().join("geo");
    fs::create_dir(&pkg).unwrap();
    fs::write(
        pkg.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"geo\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["workspace", "add", "geo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("version"));
    assert_eq!(read(&tmp.path().join("wado-workspace.toml")), ws);
}

#[test]
fn workspace_add_requires_a_package() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado-workspace.toml"),
        "[workspace]\nmembers = []\n",
    )
    .unwrap();
    fs::create_dir(tmp.path().join("empty")).unwrap();

    wado_in(tmp.path())
        .args(["workspace", "add", "empty"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wado new"));
}
