//! In-process subcommand `run()` tests — call each subcommand directly
//! and assert on the returned `CliExit`, no subprocess.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use wado_cli::args::CliExit;
use wado_cli::compile::{self, CompileOptions, OptLevel, OutputFormat};

mod common;

fn fixture(rel: &str) -> PathBuf {
    common::project_root().join(rel)
}

// `set_current_dir` is process-wide, so cwd-mutating tests serialize on this.
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

// Listed exhaustively so adding a new `CompileOptions` field forces a
// conscious decision here instead of silently defaulting.
fn hello_compile_opts(output_path: &Path, format: Option<OutputFormat>) -> CompileOptions {
    CompileOptions {
        input: fixture("example/hello.wado").to_string_lossy().into_owned(),
        output: Some(output_path.to_string_lossy().into_owned()),
        format,
        opt_level: OptLevel::default(),
        wat_to_stdout: false,
        log_level: wado_compiler::LogLevel::default(),
        target_world: None,
        skip_validation: false,
        inline_threshold: None,
        opt_iterations: None,
        allocator: None,
        lib: false,
        no_cache: false,
    }
}

#[test]
fn compile_writes_wasm_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.wasm");
    let opts = hello_compile_opts(&out, None);
    futures::executor::block_on(compile::run(opts)).expect("compile should succeed");
    let bytes = fs::read(&out).unwrap();
    assert!(bytes.len() > 4, "wasm output looked empty");
    assert_eq!(&bytes[0..4], b"\0asm", "wasm magic missing");
}

#[test]
fn compile_writes_wat_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.wat");
    let opts = hello_compile_opts(&out, None);
    futures::executor::block_on(compile::run(opts)).expect("compile should succeed");
    let text = fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("(component"),
        "WAT output missing component sexp: {text:.80}"
    );
}

#[test]
fn compile_format_overrides_extension() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let opts = hello_compile_opts(&out, Some(OutputFormat::Wat));
    futures::executor::block_on(compile::run(opts)).expect("compile should succeed");
    let text = fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("(component"),
        "expected WAT despite .txt suffix"
    );

    let out = dir.path().join("out.bin");
    let opts = hello_compile_opts(&out, Some(OutputFormat::Wasm));
    futures::executor::block_on(compile::run(opts)).expect("compile should succeed");
    let bytes = fs::read(&out).unwrap();
    assert_eq!(&bytes[0..4], b"\0asm");
}

#[test]
fn compile_succeeds_at_each_opt_level() {
    let dir = tempfile::tempdir().unwrap();
    for level in [
        OptLevel::O0,
        OptLevel::O1,
        OptLevel::O2,
        OptLevel::O3,
        OptLevel::Os,
    ] {
        let out = dir.path().join(format!("hello-{level:?}.wasm"));
        let mut opts = hello_compile_opts(&out, None);
        opts.opt_level = level;
        futures::executor::block_on(compile::run(opts))
            .unwrap_or_else(|e| panic!("compile failed at {level:?}: {:?}", e.message));
        assert!(
            fs::metadata(&out).is_ok_and(|m| m.len() > 4),
            "output missing for {level:?}"
        );
    }
}

#[test]
fn compile_helper_returns_silent_failure_on_missing_file() {
    let flags = wado_cli::compile::CompileFlags::default();
    let err: CliExit = futures::executor::block_on(wado_cli::compile::compile(
        "/definitely/does/not/exist.wado",
        &flags,
    ))
    .expect_err("expected CliExit");
    // Host already printed "Error reading ...", so the CliExit is empty.
    assert_eq!(err.exit_code, 1);
    assert!(err.message.is_empty(), "got {:?}", err.message);
}
