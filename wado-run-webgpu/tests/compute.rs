//! The runner, end to end: compile a Wado program that dispatches a WGSL
//! compute shader, run it, and read what the GPU wrote back.

use std::path::PathBuf;
use std::process::Command;
use std::sync::LazyLock;

/// The `wado` the runner compiles with, as `WADO` names it. A local run builds
/// the repository's own, once for the whole test binary.
static WADO: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Some(wado) = std::env::var_os("WADO") {
        return PathBuf::from(wado);
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate sits in the repository")
        .to_path_buf();
    let status = Command::new("cargo")
        .args(["build", "--quiet", "--bin", "wado"])
        .current_dir(&repo)
        .status()
        .expect("cargo build");
    assert!(status.success(), "building wado failed");
    // Cargo reads a relative `CARGO_TARGET_DIR` from the directory it was run
    // in, which is `repo` above and not this process's own.
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| repo.join("target"), |dir| repo.join(dir));
    let wado = target.join("debug/wado");
    assert!(wado.is_file(), "no wado at {}", wado.display());
    wado
});

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn runner() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wado-run-webgpu"));
    command.env("WADO", &*WADO);
    command
}

fn run(args: &[&str]) -> std::process::Output {
    runner()
        .args(args)
        .output()
        .expect("running wado-run-webgpu")
}

#[test]
fn a_compute_shader_writes_what_the_guest_reads_back() {
    let output = run(&[fixture("compute_double.wado").to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("compute ok"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(output.status.success(), "stderr: {stderr}");
}

#[test]
fn an_unknown_option_is_refused_before_anything_is_compiled() {
    let output = run(&["--frobnicate", "x.wado"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--frobnicate"), "stderr: {stderr}");
}

/// With no driver to load, wgpu finds no adapter, and the guest would only see
/// `request-adapter` answer `none`. `VK_DRIVER_FILES` is how a Linux machine
/// can be put in that state on purpose.
#[test]
#[cfg(target_os = "linux")]
fn a_machine_without_an_adapter_is_told_what_to_install() {
    let output = runner()
        .env("VK_DRIVER_FILES", "/nonexistent.json")
        .arg(fixture("compute_double.wado"))
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("no GPU adapter found"), "stderr: {stderr}");
    assert!(stderr.contains("mesa-vulkan-drivers"), "stderr: {stderr}");
}

#[test]
fn the_usage_names_the_subcommand_it_serves() {
    let output = run(&["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wado run-webgpu"), "stdout: {stdout}");
}
