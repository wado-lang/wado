//! Integration tests for wado.toml manifest discovery and CLI integration.

use predicates::prelude::*;
use std::fs;

mod common;
use common::wado_in;

#[test]
fn test_init_creates_manifest() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "my-app"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Created wado.toml"));

    let content = fs::read_to_string(tmp.path().join("wado.toml")).unwrap();
    assert!(content.contains("name = \"my-app\""));
    assert!(content.contains("version = \"0.1.0\""));
    assert!(content.contains("command = \"src/main.wado\""));
}

#[test]
fn test_init_with_namespace() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "my-app", "--namespace", "myorg"])
        .assert()
        .success();

    let content = fs::read_to_string(tmp.path().join("wado.toml")).unwrap();
    assert!(content.contains("namespace = \"myorg\""));
    assert!(content.contains("name = \"my-app\""));
}

#[test]
fn test_init_fails_when_manifest_exists() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "my-app"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--force"));
}

#[test]
fn test_init_force_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nname = \"old\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["init", "--name", "new-app", "--force"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Created wado.toml"));

    let content = fs::read_to_string(tmp.path().join("wado.toml")).unwrap();
    assert!(content.contains("name = \"new-app\""));
}

#[test]
fn test_init_requires_name() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--name is required"));
}

#[test]
fn test_init_help() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .args(["init", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado init"));
}

#[test]
fn test_compile_with_manifest() {
    let tmp = tempfile::tempdir().unwrap();

    // Create project structure
    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let toml = r#"[package]
name = "test-app"
version = "0.1.0"
command = "src/main.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello from manifest");
}
"#;
    fs::write(src_dir.join("main.wado"), source).unwrap();

    let output_path = tmp.path().join("out.wasm");

    wado_in(tmp.path())
        .args(["compile", "-o", output_path.to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("Generated:"));

    assert!(output_path.exists());
}

#[test]
fn test_compile_file_arg_overrides_manifest() {
    let tmp = tempfile::tempdir().unwrap();

    // Create a wado.toml pointing to a nonexistent file
    let toml = r#"[package]
name = "test-app"
version = "0.1.0"
command = "nonexistent.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    // Create an actual source file
    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello");
}
"#;
    fs::write(tmp.path().join("actual.wado"), source).unwrap();

    let output_path = tmp.path().join("out.wasm");

    // Explicit file argument should override the manifest entry point
    wado_in(tmp.path())
        .args([
            "compile",
            "-o",
            output_path.to_str().unwrap(),
            "actual.wado",
        ])
        .assert()
        .success();

    assert!(output_path.exists());
}

#[test]
fn test_run_with_manifest() {
    let tmp = tempfile::tempdir().unwrap();

    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let toml = r#"[package]
name = "test-app"
version = "0.1.0"
command = "src/main.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello from manifest run");
}
"#;
    fs::write(src_dir.join("main.wado"), source).unwrap();

    wado_in(tmp.path())
        .args(["run", "--no-dir"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from manifest run"));
}

#[test]
fn test_compile_no_manifest_no_file() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .arg("compile")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no input file specified"));
}

#[test]
fn test_compile_manifest_missing_command() {
    let tmp = tempfile::tempdir().unwrap();

    // Manifest with lib but no command
    let toml = r#"[package]
name = "lib-only"
version = "0.1.0"
lib = "src/lib.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    wado_in(tmp.path())
        .arg("compile")
        .assert()
        .failure()
        .stderr(predicate::str::contains("[package].command is not set"));
}

#[test]
fn test_serve_manifest_missing_service() {
    let tmp = tempfile::tempdir().unwrap();

    let toml = r#"[package]
name = "cli-only"
version = "0.1.0"
command = "src/main.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    wado_in(tmp.path())
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicate::str::contains("[package].service is not set"));
}

#[test]
fn test_compile_from_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();

    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let toml = r#"[package]
name = "test-app"
version = "0.1.0"
command = "src/main.wado"
"#;
    fs::write(tmp.path().join("wado.toml"), toml).unwrap();

    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello from subdir");
}
"#;
    fs::write(src_dir.join("main.wado"), source).unwrap();

    let output_path = tmp.path().join("out.wasm");

    // Run from src/ subdirectory — should discover wado.toml in parent
    wado_in(&src_dir)
        .args(["compile", "-o", output_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(output_path.exists());
}

#[test]
fn test_help_shows_init_command() {
    let tmp = tempfile::tempdir().unwrap();

    wado_in(tmp.path())
        .arg("--help")
        .assert()
        .success()
        .stderr(predicate::str::contains("init"));
}
