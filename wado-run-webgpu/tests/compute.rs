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
    // Below `--log-level info` the adapter goes unnamed.
    assert!(!stderr.contains("request-adapter"), "stderr: {stderr}");
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

/// The names of this machine's adapters, as the refusal of a name nothing
/// matches lists them, so no test assumes which GPU or driver is installed.
fn adapter_names() -> Vec<String> {
    let output = run(&["--gpu-adapter", "no-such-gpu", "app.wado"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    let (_, listing) = stderr
        .split_once("The adapters here:\n")
        .unwrap_or_else(|| panic!("no listing in stderr: {stderr}"));
    listing
        .lines()
        .map(|line| {
            let (name, _) = line.trim_start().rsplit_once(" (").expect("described");
            name.to_owned()
        })
        .collect()
}

#[test]
fn the_named_adapter_is_the_one_the_guest_gets_and_info_says_which() {
    let names = adapter_names();
    let name = names
        .iter()
        .find(|name| names.iter().filter(|other| other == name).count() == 1)
        .expect("an adapter whose name no other shares");
    let output = run(&[
        "--gpu-adapter",
        &name.to_uppercase(),
        "--log-level",
        "info",
        fixture("compute_double.wado").to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("compute ok"), "stdout: {stdout}");
    assert!(
        stderr.contains(&format!("info: request-adapter: {name} (")),
        "stderr: {stderr}"
    );
}

#[test]
fn an_adapter_name_nothing_matches_is_refused_with_the_names_there_are() {
    assert!(!adapter_names().is_empty());
}

/// The runner reads the level itself and hands it to `wado compile`, so the
/// two must accept the same spellings.
#[test]
fn the_log_level_spellings_are_the_ones_wado_takes() {
    for level in [
        "debug", "info", "warn", "warning", "error", "off", "none", "INFO", "loud",
    ] {
        let runner_accepts = run(&["--log-level", level, "--help"]).status.success();
        let wado_accepts = Command::new(&*WADO)
            .args(["check", "--log-level", level])
            .arg(fixture("compute_double.wado"))
            .output()
            .expect("running wado check")
            .status
            .success();
        assert_eq!(runner_accepts, wado_accepts, "--log-level {level}");
    }
}

#[test]
fn the_usage_names_the_subcommand_it_serves() {
    let output = run(&["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wado run-webgpu"), "stdout: {stdout}");
}
