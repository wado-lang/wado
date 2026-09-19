//! `wado --list` and `wado help <name>` over the `wado-<name>` files on `PATH`.
//! See `docs/wep-2026-09-19-external-subcommands.md`.

use std::path::Path;

use predicates::prelude::*;

use crate::common::{wado, wado_in};

/// A `wado-<name>` that prints its arguments, so a test can tell whether it ran.
fn install(dir: &Path, name: &str) {
    install_exiting(dir, name, 0);
}

/// The same, leaving `code` as its exit status.
fn install_exiting(dir: &Path, name: &str, code: i32) {
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"ran $0 argv:$*\"\nexit {code}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn list_names_an_external_and_marks_one_a_builtin_shadows() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-run-with-webgpu");
    install(tmp.path(), "wado-run");

    wado()
        .env("PATH", tmp.path())
        .arg("--list")
        .assert()
        .success()
        .stdout(predicate::str::contains("run-with-webgpu"))
        .stdout(predicate::str::contains("shadowed by the builtin"));
}

/// The child reads the command line itself, so `wado` parses none of it, and
/// it is the child's exit status that the shell sees.
#[test]
fn an_external_receives_the_rest_of_the_command_line_and_owns_the_exit_status() {
    let tmp = tempfile::tempdir().unwrap();
    install_exiting(tmp.path(), "wado-run-with-webgpu", 7);

    wado()
        .env("PATH", tmp.path())
        .args(["run-with-webgpu", "app.wado", "--dir", ".", "-O2"])
        .assert()
        .code(7)
        .stdout(predicate::str::contains("argv:app.wado --dir . -O2"));
}

#[test]
fn a_builtin_runs_even_when_path_offers_its_name() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-init");

    wado_in(tmp.path())
        .env("PATH", tmp.path())
        .args(["init", "--name", "shadowed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran ").not());
    assert!(tmp.path().join("wado.toml").exists());
}

#[test]
fn an_unknown_command_says_where_it_looked() {
    wado()
        .env("PATH", "")
        .arg("run-with-webgpu")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unknown command 'run-with-webgpu' (no 'wado-run-with-webgpu' on PATH)",
        ));
}

#[test]
fn help_hands_an_external_its_own_help() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-run-with-webgpu");

    wado()
        .env("PATH", tmp.path())
        .args(["help", "run-with-webgpu"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran "))
        .stdout(predicate::str::contains("wado-run-with-webgpu"));
}

#[test]
fn help_answers_for_a_builtin_without_consulting_path() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-run");

    wado()
        .env("PATH", tmp.path())
        .args(["help", "run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran ").not())
        .stderr(predicate::str::contains(
            "Compile and run a Wado CLI program",
        ));
}

#[test]
fn help_rejects_a_name_that_is_on_no_path() {
    wado()
        .env("PATH", "")
        .args(["help", "run-with-webgpu"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown command"));
}

/// An empty `PATH` entry is the current directory to the operating system's
/// own search, and a relative entry resolves against it. Neither may supply a
/// subcommand, or the directory a user stands in decides what `wado` runs.
#[test]
fn neither_an_empty_nor_a_relative_path_entry_supplies_a_subcommand() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-run-with-webgpu");

    for path in ["/usr/bin::/bin", "/usr/bin:.:/bin"] {
        wado_in(tmp.path())
            .env("PATH", path)
            .args(["help", "run-with-webgpu"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unknown command"));
    }
}

/// A file on `PATH` whose name is not spelled the way a builtin is was never
/// meant as a subcommand, so it is neither listed nor run.
#[test]
fn a_file_outside_the_builtin_name_shape_is_not_a_subcommand() {
    let tmp = tempfile::tempdir().unwrap();
    install(tmp.path(), "wado-MixedCase");

    wado()
        .env("PATH", tmp.path())
        .arg("--list")
        .assert()
        .success()
        .stdout(predicate::str::contains("MixedCase").not());
}
