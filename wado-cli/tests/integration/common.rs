//! Shared helpers for wado-cli integration tests.
//!
//! Each test file pulls these in via `mod common;`. The file also compiles
//! as its own (empty) integration test binary; `#![allow(dead_code)]`
//! quiets warnings when an individual test file uses only a subset.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Repository root (parent of `wado-cli/`).
pub fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Path to the freshly-built `wado` binary.
pub fn wado_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("wado").into()
}

/// `wado` invocation rooted at the project root. Use this when the test
/// drives an example file (e.g. `example/hello.wado`) by relative path.
pub fn wado() -> assert_cmd::Command {
    let mut cmd = Command::new(wado_bin());
    cmd.current_dir(project_root());
    cmd.into()
}

/// `wado` invocation rooted at `dir`. Use this when the test creates a
/// self-contained workspace (`wado.toml`, sources) under a tempdir.
pub fn wado_in(dir: &Path) -> assert_cmd::Command {
    let mut cmd = Command::new(wado_bin());
    cmd.current_dir(dir);
    cmd.into()
}

/// Single-threaded Tokio runtime for tests that drive async APIs from a
/// synchronous `#[test]`.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A one-schema Kiln project under `root`: `generator` turns `src/schema.bin`
/// into a `hello()` that `src/main.wado` prints.
pub fn write_kiln_project(root: &Path, package: &str, generator: &str, schema: &[u8]) {
    std::fs::write(
        root.join("wado.toml"),
        format!(
            "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/gen.wado"), generator).unwrap();
    std::fs::write(root.join("src/schema.bin"), schema).unwrap();
    std::fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "./schema.bin"
    with { generator: { module: "./gen.wado" } };

export fn run() with Stdout {
    println(`${hello()}`);
}
"#,
    )
    .unwrap();
}

/// The custom sections `(name, payload)` of a compiled component at `wasm_path`.
pub fn custom_sections(wasm_path: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(wasm_path).unwrap();
    let mut out = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        if let Ok(wasmparser::Payload::CustomSection(reader)) = payload {
            out.push((reader.name().to_string(), reader.data().to_vec()));
        }
    }
    out
}
