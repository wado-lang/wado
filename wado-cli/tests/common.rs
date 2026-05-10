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
