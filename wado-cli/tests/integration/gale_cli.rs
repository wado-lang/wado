//! End-to-end test for the `gale` CLI (`package-gale/src/main.wado`).
//!
//! Every in-tree consumer of Gale now drives the generator through
//! Kiln (see `package-gale/tests/driver_*_test.wado`), but the standalone
//! `wado run package-gale -- gen <Grammar.g4>` CLI is still exposed for
//! ad-hoc use. This test pins that surface so a regression in main.wado
//! is caught even though no production caller depends on it.

use predicates::prelude::*;

use crate::common::wado;

// `-O0`: the surface under test is the CLI's, not the optimizer's, and Gale is
// large enough that the default `-O2` compile of it takes 7x as long (164s
// against 22s), which made these two the tail of the whole suite.
#[test]
fn gale_gen_calculator_emits_generated_parser() {
    wado()
        .args([
            "run",
            "-O0",
            "package-gale/src/main.wado",
            "gen",
            "package-gale/tests/grammars/calculator.g4",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("#![generated(by = \"gale\""))
        .stdout(predicate::str::contains("calculator.g4"))
        .stdout(predicate::str::contains(
            "pub fn parse<S: AsStrSlice>(input: S",
        ))
        .stdout(predicate::str::contains(
            "pub fn tokenize<S: AsStrSlice>(input: S",
        ));
}

#[test]
fn gale_gen_highlight_emits_highlight_function() {
    // A query named by `--highlights` turns the highlighter on.
    wado()
        .args([
            "run",
            "-O0",
            "package-gale/src/main.wado",
            "gen",
            "--highlights",
            "package-gale/tests/grammars/calculator.highlights.scm",
            "package-gale/tests/grammars/calculator.g4",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("#![generated(by = \"gale\""))
        .stdout(predicate::str::contains(
            "pub fn highlight<S: AsStrSlice>(input: S",
        ));
}
