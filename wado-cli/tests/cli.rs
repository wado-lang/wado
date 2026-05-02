//! End-to-end tests for wado CLI
//!
//! Tests the CLI interface including argument parsing, subcommands,
//! and integration with the compiler.

use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Get the project root directory (parent of wado-cli)
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
fn test_compile_help() {
    wado()
        .args(["compile", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado compile"));
}

#[test]
fn test_compile_missing_input() {
    // The repo root carries a `wado.toml` with no `[package].command` so the
    // resolver can't synthesise an input file; the user gets a focused error.
    wado()
        .arg("compile")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "wado.toml found but [package].command is not set",
        ));
}

#[test]
fn test_compile_file_not_found() {
    wado()
        .args(["compile", "nonexistent.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nonexistent.wado"));
}

#[test]
fn test_compile_output_wasm() {
    let temp_dir = std::env::temp_dir();
    let output_path = temp_dir.join("test_compile_output.wasm");

    // Clean up before test
    let _ = fs::remove_file(&output_path);

    wado()
        .args([
            "compile",
            "-o",
            output_path.to_str().unwrap(),
            "example/hello.wado",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Generated:"));

    // Verify file was created
    assert!(output_path.exists(), "Output file should exist");

    // Verify it's a valid Wasm file (starts with magic bytes)
    let content = fs::read(&output_path).unwrap();
    assert!(content.len() > 4, "Wasm file should have content");
    // Component model starts with \0asm but version differs from core wasm

    // Clean up
    let _ = fs::remove_file(&output_path);
}

#[test]
fn test_compile_output_wat() {
    let temp_dir = std::env::temp_dir();
    let output_path = temp_dir.join("test_compile_output.wat");

    // Clean up before test
    let _ = fs::remove_file(&output_path);

    wado()
        .args([
            "compile",
            "-o",
            output_path.to_str().unwrap(),
            "example/hello.wado",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Generated:"));

    // Verify file was created with WAT content
    let content = fs::read_to_string(&output_path).unwrap();
    assert!(
        content.contains("(component"),
        "WAT file should contain component"
    );

    // Clean up
    let _ = fs::remove_file(&output_path);
}

#[test]
fn test_compile_format_wat() {
    let temp_dir = std::env::temp_dir();
    let output_path = temp_dir.join("test_compile_format.txt");

    // Clean up before test
    let _ = fs::remove_file(&output_path);

    wado()
        .args([
            "compile",
            "--format",
            "wat",
            "-o",
            output_path.to_str().unwrap(),
            "example/hello.wado",
        ])
        .assert()
        .success();

    // Verify WAT content even with non-standard extension
    let content = fs::read_to_string(&output_path).unwrap();
    assert!(
        content.contains("(component"),
        "Should be WAT format regardless of extension"
    );

    // Clean up
    let _ = fs::remove_file(&output_path);
}

#[test]
fn test_compile_format_wasm() {
    let temp_dir = std::env::temp_dir();
    let output_path = temp_dir.join("test_compile_format.bin");

    // Clean up before test
    let _ = fs::remove_file(&output_path);

    wado()
        .args([
            "compile",
            "--format",
            "wasm",
            "-o",
            output_path.to_str().unwrap(),
            "example/hello.wado",
        ])
        .assert()
        .success();

    // Verify binary content even with non-standard extension
    let content = fs::read(&output_path).unwrap();
    assert!(content.len() > 4, "Wasm file should have content");

    // Clean up
    let _ = fs::remove_file(&output_path);
}

#[test]
fn test_compile_wat_to_stdout() {
    wado()
        .args(["compile", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_compile_invalid_format() {
    wado()
        .args(["compile", "--format", "invalid", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown format"));
}

#[test]
fn test_compile_unknown_option() {
    wado()
        .args(["compile", "--unknown", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid option"));
}

#[test]
fn test_compile_opt_level_o0() {
    wado()
        .args(["compile", "-O0", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_compile_opt_level_o2() {
    wado()
        .args(["compile", "-O2", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_compile_opt_level_os() {
    wado()
        .args(["compile", "-Os", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_compile_opt_level_invalid() {
    wado()
        .args(["compile", "-Ox", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown optimization level"));
}

#[test]
fn test_run_help() {
    wado()
        .args(["run", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado run"));
}

#[test]
fn test_run_missing_input() {
    // Same story as `test_compile_missing_input`: the repo root has a
    // `wado.toml` without `[package].command`, so the entry-point resolver
    // surfaces a more specific failure.
    wado()
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "wado.toml found but [package].command is not set",
        ));
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
fn test_run_file_not_found() {
    wado()
        .args(["run", "nonexistent.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nonexistent.wado"));
}

#[test]
fn test_run_unknown_option() {
    wado()
        .args(["run", "--unknown", "example/hello.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid option"));
}

#[test]
fn test_test_help() {
    wado()
        .args(["test", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado test"))
        .stderr(predicate::str::contains("--filter"));
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
    // --filter is a path-based wildcard (WEP 2026-05-02). `*` does not cross
    // path separators (consistent with shell glob and `.gitignore`); use
    // `**` to match across directories.
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

#[test]
fn test_test_parallel_invalid() {
    // Test with invalid parallel count
    wado()
        .args([
            "test",
            "-p",
            "0",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--parallel requires a positive integer",
        ));
}

#[test]
fn test_test_parallel_non_numeric() {
    // Test with non-numeric parallel count
    wado()
        .args([
            "test",
            "-p",
            "abc",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--parallel requires a positive integer",
        ));
}

#[test]
fn test_test_help_shows_parallel_option() {
    wado()
        .args(["test", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("-p, --parallel"));
}
