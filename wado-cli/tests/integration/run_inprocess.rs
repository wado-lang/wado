//! In-process subcommand `run()` tests — call each subcommand directly
//! and assert on the returned `CliExit`, no subprocess.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common::project_root;
use wado_cli::args::CliExit;
use wado_cli::compile::{self, CompileOptions, OutputFormat};
use wado_cli::knobs::{CompileKnobs, EmbedOptions, HostKnobs, OptLevel};

fn fixture(rel: &str) -> PathBuf {
    project_root().join(rel)
}

// Listed exhaustively so adding a new `CompileOptions` field forces a
// conscious decision here instead of silently defaulting.
fn hello_compile_opts(output_path: &Path, format: Option<OutputFormat>) -> CompileOptions {
    CompileOptions {
        input: fixture("example/hello.wado").to_string_lossy().into_owned(),
        output: Some(output_path.to_string_lossy().into_owned()),
        format,
        wat_to_stdout: false,
        knobs: CompileKnobs::default(),
        host: HostKnobs::default(),
        embed: EmbedOptions::default(),
        target_world: None,
        lib_world: None,
        // Input is an explicit `.wado` file, so this is not manifest-driven.
        manifest_driven: false,
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
        opts.knobs.opt_level = level;
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
