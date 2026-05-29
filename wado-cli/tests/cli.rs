//! End-to-end tests for wado CLI
//!
//! Tests the CLI interface including argument parsing, subcommands,
//! and integration with the compiler.

use predicates::prelude::*;

mod common;
use common::wado;

#[test]
fn test_help() {
    wado()
        .arg("--help")
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado <command>"))
        .stderr(predicate::str::contains("compile"))
        .stderr(predicate::str::contains("run"));
}

#[test]
fn test_version() {
    wado()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("wado "))
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_no_args_shows_usage() {
    wado()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: wado"));
}

#[test]
fn test_unknown_command() {
    wado()
        .arg("unknown")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown command"));
}

#[test]
fn test_compile_wat_to_stdout() {
    // Subprocess-only because `--wat-to-stdout` writes to actual stdout;
    // capturing it in-process would require redirecting std file
    // descriptors, which we have not done yet. The file-output and
    // opt-level variants moved to `tests/run_inprocess.rs`.
    wado()
        .args(["compile", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_run_hello() {
    wado()
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, world!"));
}

#[test]
fn test_test_passing() {
    wado()
        .args(["test", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_failing() {
    wado()
        .args(["test", "wado-cli/tests/fixtures/test_fail.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("1 passed, 1 failed"));
}

#[test]
fn test_test_filter_keeps_matching_path() {
    // --filter is a path-based wildcard. `*` does not cross path
    // separators (consistent with shell glob and `.gitignore`); use `**`
    // to match across directories.
    wado()
        .args([
            "test",
            "--filter",
            "**/test_decl.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_filter_repeatable_keeps_union_of_matches() {
    // `--filter` is repeatable: a path is kept when any pattern matches.
    wado()
        .args([
            "test",
            "--filter",
            "**/test_decl.wado",
            "--filter",
            "**/never_matches.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_compile_failure_reported_on_compile_axis() {
    // A file that fails to compile must (a) not abort the run, (b) be reported
    // on the `compile` axis, and (c) cause a non-zero exit. The peer file
    // still compiles and its passing test still runs.
    wado()
        .args([
            "test",
            "wado-cli/tests/fixtures/test_compile_error.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("compile: 1 ok, 1 failed"))
        .stdout(predicate::str::contains("test:    2 passed, 0 failed"))
        .stdout(predicate::str::contains("compile failures:"))
        .stdout(predicate::str::contains(
            "wado-cli/tests/fixtures/test_compile_error.wado",
        ));
}

#[test]
fn test_test_no_run_skips_phase_two() {
    // `--no-run` must (a) compile the file successfully — Phase 1 still
    // runs, which is where Kiln caches get refreshed; (b) skip the
    // wasmtime execution phase so the two tests in `test_decl.wado` do
    // not run; (c) exit 0. The compile axis shows 1 ok and the test
    // axis shows zero passes.
    wado()
        .args([
            "test",
            "--no-run",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("compile: 1 ok, 0 failed"))
        .stdout(predicate::str::contains("test:    0 passed, 0 failed"));
}

#[test]
fn test_test_no_run_surfaces_todo_compile_errors() {
    // A `#![TODO]` module whose expected compile error fires is
    // module-level TODO-pending. Under `--no-run` we can't observe
    // test-level TODO resolution, but the module-level outcome is
    // detectable at compile time and must stay visible — both as a
    // `todo: N pending` summary line and in the consolidated
    // `TODO tests:` section, matching the normal-run path's parity.
    wado()
        .args([
            "test",
            "--no-run",
            "wado-compiler/tests/fixtures/module_attr_todo_compile_error.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("compile: 1 ok, 0 failed"))
        .stdout(predicate::str::contains("todo:    1 pending"))
        .stdout(predicate::str::contains(
            "wado-compiler/tests/fixtures/module_attr_todo_compile_error.wado",
        ))
        .stdout(predicate::str::contains("#![TODO] module"));
}

#[test]
fn test_test_filter_drops_non_matching_path() {
    // No path matches the pattern, so no test files remain to run.
    wado()
        .args([
            "test",
            "--filter",
            "**/does_not_match.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("No .wado files match --filter"));
}

#[test]
fn test_test_parallel_option() {
    // Test with explicit parallel count
    wado()
        .args([
            "test",
            "-p",
            "2",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_parallel_long_option() {
    // Test with long option
    wado()
        .args([
            "test",
            "--parallel",
            "1",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}
