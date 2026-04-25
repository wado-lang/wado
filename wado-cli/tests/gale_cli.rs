//! End-to-end test for the `gale` CLI (`package-gale/src/main.wado`).
//!
//! Every in-tree consumer of Gale now drives the generator through
//! Kiln (see `package-gale/tests/driver_*_test.wado`), but the standalone
//! `wado run package-gale -- gen <Grammar.g4>` CLI is still exposed for
//! ad-hoc use. This test pins that surface so a regression in main.wado
//! is caught even though no production caller depends on it.

#![allow(unused_crate_dependencies)]

use std::path::PathBuf;
use std::process::Command;

use predicates::prelude::*;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn wado() -> assert_cmd::Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("wado"));
    cmd.current_dir(project_root());
    cmd.into()
}

#[test]
fn gale_gen_calculator_emits_generated_parser() {
    wado()
        .args([
            "run",
            "package-gale/src/main.wado",
            "gen",
            "package-gale/tests/grammars/calculator.g4",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("#![generated(by = \"gale\""))
        .stdout(predicate::str::contains("calculator.g4"))
        .stdout(predicate::str::contains("pub fn parse("))
        .stdout(predicate::str::contains("pub fn tokenize("));
}

#[test]
fn gale_gen_highlight_emits_highlight_function() {
    wado()
        .args([
            "run",
            "package-gale/src/main.wado",
            "gen",
            "--highlight",
            "package-gale/tests/grammars/calculator.g4",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("#![generated(by = \"gale\""))
        .stdout(predicate::str::contains("pub fn highlight("));
}
