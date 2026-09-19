//! `wado --list` and `wado help <name>` over the `wado-<name>` files on `PATH`.
//! See `docs/wep-2026-09-19-external-subcommands.md`.

use std::path::Path;

use predicates::prelude::*;

use crate::common::{wado, wado_in};

/// A `wado-<name>` that prints its arguments, so a test can tell whether it ran.
fn install(dir: &Path, name: &str) {
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\necho \"ran $0\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
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
