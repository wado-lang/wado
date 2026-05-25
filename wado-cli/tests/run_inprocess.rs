//! In-process `run()` tests.
//!
//! Exercises subcommand `run()` functions directly without spawning a
//! subprocess. These are only possible because every `run()` returns
//! `Result<(), CliExit>` instead of calling `process::exit(...)`; the
//! exit-code propagates through the result, so the test can assert on it
//! without forking.

use std::env;
use std::fs;
use std::sync::{Mutex, OnceLock};

use wado_cli::args::CliExit;

/// Several tests below `env::set_current_dir` into a temporary directory.
/// `set_current_dir` is process-wide, so without serialization concurrent
/// cargo-test threads would race. The Mutex is held for the duration of
/// each test that touches cwd.
fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn with_tempdir<F: FnOnce() -> R, R>(f: F) -> R {
    let guard = cwd_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let prev = env::current_dir().unwrap();
    env::set_current_dir(dir.path()).unwrap();
    let result = f();
    env::set_current_dir(prev).unwrap();
    drop(guard);
    result
}

#[test]
fn init_run_creates_manifest_in_cwd() {
    with_tempdir(|| {
        let opts = wado_cli::init::InitOptions {
            name: "demo".to_string(),
            namespace: None,
            force: false,
        };
        let result = wado_cli::init::run(opts);
        assert!(result.is_ok(), "init::run should succeed in empty cwd");
        let content = fs::read_to_string("wado.toml").unwrap();
        assert!(content.contains("name = \"demo\""));
    });
}

#[test]
fn init_run_refuses_to_overwrite_without_force() {
    with_tempdir(|| {
        fs::write("wado.toml", "pre-existing").unwrap();
        let opts = wado_cli::init::InitOptions {
            name: "demo".to_string(),
            namespace: None,
            force: false,
        };
        let err = wado_cli::init::run(opts).expect_err("expected CliExit");
        assert_eq!(err.exit_code, 1);
        assert!(
            err.message.contains("already exists"),
            "unexpected message: {:?}",
            err.message
        );
        // File is untouched.
        assert_eq!(fs::read_to_string("wado.toml").unwrap(), "pre-existing");
    });
}

#[test]
fn init_run_force_overwrites() {
    with_tempdir(|| {
        fs::write("wado.toml", "pre-existing").unwrap();
        let opts = wado_cli::init::InitOptions {
            name: "demo".to_string(),
            namespace: None,
            force: true,
        };
        wado_cli::init::run(opts).expect("force should succeed");
        let content = fs::read_to_string("wado.toml").unwrap();
        assert!(content.contains("name = \"demo\""));
    });
}

#[test]
fn compile_helper_returns_silent_failure_on_missing_file() {
    // `compile::compile` is the helper shared by `run` / `serve` / `test`.
    // A missing source file used to abort the process; now the caller can
    // observe the failure and decide how to react.
    let flags = wado_cli::compile::CompileFlags::default();
    let err: CliExit = futures::executor::block_on(wado_cli::compile::compile(
        "/definitely/does/not/exist.wado",
        &flags,
    ))
    .expect_err("expected CliExit");
    // The diagnostics ("Error reading ...") were already printed by the
    // host, so the failure is silent: just an exit code.
    assert_eq!(err.exit_code, 1);
    assert!(
        err.message.is_empty(),
        "expected silent failure (no message), got {:?}",
        err.message
    );
}
