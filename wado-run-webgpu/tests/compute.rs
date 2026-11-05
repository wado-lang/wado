//! The runner, end to end: compile a Wado program that dispatches a WGSL
//! compute shader, run it, and read what the GPU wrote back. Loam's WebGPU
//! backend is run here too, the one place a device is.

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

fn served() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../wado-host/tests/fixtures/served.wado")
}

/// `served.wado`'s stdout under `wado run`.
fn under_wado_run(args: &[&str]) -> String {
    let output = Command::new(&*WADO)
        .arg("run")
        .args(args)
        .arg(served())
        .output()
        .expect("running wado");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    String::from_utf8(output.stdout).expect("the program prints text")
}

#[test]
fn a_program_reaches_what_it_reaches_under_wado_run() {
    for args in [
        ["-D", "ANSWER=42", "--allocator", "freelist"].as_slice(),
        [
            "--deny-process",
            "--collector",
            "drc",
            "--gc-heap-initial",
            "64m",
        ]
        .as_slice(),
    ] {
        let output = runner()
            .args(args)
            .arg(served())
            .output()
            .expect("running wado-run-webgpu");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "stderr: {stderr}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            under_wado_run(args)
        );
        assert_eq!(stderr, "");
    }
}

#[test]
fn a_fuel_report_is_the_one_wado_run_prints() {
    let output = runner()
        .arg("--report-fuel")
        .arg(served())
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stderr.starts_with("fuel "), "stderr: {stderr}");
}

#[test]
fn with_no_input_the_entry_comes_from_the_manifest() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"main.wado\"\n",
    )
    .expect("writing the manifest");
    std::fs::copy(served(), dir.path().join("main.wado")).expect("copying the program");

    let output = runner()
        .current_dir(dir.path())
        .output()
        .expect("running wado-run-webgpu");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("main.wado"), "stdout: {stdout}");
}

#[test]
fn a_compile_error_ends_the_run_with_the_compilers_own_report() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("broken.wado");
    std::fs::write(&path, "export fn run() { missing(); }\n").expect("writing the program");

    let output = run(&[path.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("missing"), "stderr: {stderr}");
}

#[test]
fn no_input_and_no_manifest_says_so_without_wado_builds_usage() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = runner()
        .current_dir(dir.path())
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("no wado.toml found"), "stderr: {stderr}");
    assert!(!stderr.contains("wado build"), "stderr: {stderr}");
}

/// An executable script in a directory of its own, named `name`.
#[cfg(unix)]
fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("writing the script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("marking it executable");
    path
}

#[cfg(unix)]
#[test]
fn a_wado_killed_by_a_signal_is_named_rather_than_left_silent() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let killed = script(dir.path(), "killed", "kill -9 $$");
    let output = runner()
        .env("WADO", killed)
        .arg(served())
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("signal"), "stderr: {stderr}");
}

/// With no `WADO`, the search follows `path_search`: an empty `PATH` entry would
/// let the directory a user stands in decide what compiles.
#[cfg(unix)]
#[test]
fn a_wado_in_the_current_directory_is_not_run() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    script(dir.path(), "wado", "echo ran the wado here >&2");
    let output = runner()
        .env_remove("WADO")
        .env("PATH", ":")
        .current_dir(dir.path())
        .arg(served())
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(!stderr.contains("ran the wado here"), "stderr: {stderr}");
}

#[test]
fn a_directory_that_cannot_be_opened_is_reported_as_wado_run_reports_it() {
    let served = served();
    let args = ["--dir", "/nonexistent/grant", served.to_str().unwrap()];
    let here = run(&args);
    let there = Command::new(&*WADO)
        .arg("run")
        .args(args)
        .output()
        .expect("running wado");

    assert!(!here.status.success());
    assert_eq!(here.status.code(), there.status.code());
    assert_eq!(
        String::from_utf8_lossy(&here.stderr),
        String::from_utf8_lossy(&there.stderr)
    );
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
fn a_guest_profile_is_written_where_the_option_names() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("profile.json");
    let output = run(&[
        &format!("--profile=guest,{},1", path.display()),
        fixture("compute_double.wado").to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    let profile = std::fs::read_to_string(&path).expect("the profile was written");
    assert!(profile.contains("\"threads\""), "{profile}");
}

#[test]
fn a_guest_profile_stops_at_its_budget_and_is_still_written() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("profile.json");
    let output = runner()
        .env("WADO_PROFILE_MAX_SECS", "1")
        .args([
            &format!("--profile=guest,{},1", path.display()),
            fixture("spins_forever.wado").to_str().unwrap(),
        ])
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("WADO_PROFILE_MAX_SECS"), "stderr: {stderr}");
    let profile = std::fs::read_to_string(&path).expect("the profile was written");
    assert!(profile.contains("\"threads\""), "{profile}");
}

/// A profile that fails to write as the run ends does not replace what the run
/// reported: `/dev/full` opens, and every write to it fails.
#[cfg(target_os = "linux")]
#[test]
fn the_runs_own_outcome_comes_before_a_profile_it_could_not_write() {
    let output = runner()
        .env("WADO_PROFILE_MAX_SECS", "1")
        .args([
            "--profile=guest,/dev/full,1",
            fixture("spins_forever.wado").to_str().unwrap(),
        ])
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("WADO_PROFILE_MAX_SECS"), "stderr: {stderr}");
    assert!(
        stderr.contains("No space left on device"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_budget_not_in_whole_seconds_is_refused_before_the_run() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("profile.json");
    let output = runner()
        .env("WADO_PROFILE_MAX_SECS", "1.5")
        .args([
            &format!("--profile=guest,{},1", path.display()),
            fixture("spins_forever.wado").to_str().unwrap(),
        ])
        .output()
        .expect("running wado-run-webgpu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("not '1.5'"), "stderr: {stderr}");
}

#[test]
fn a_profile_path_that_cannot_be_written_fails_before_the_run() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("missing/profile.json");
    let output = run(&[
        &format!("--profile=guest,{},1", path.display()),
        fixture("compute_double.wado").to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(!stdout.contains("compute ok"), "stdout: {stdout}");
    assert!(
        stderr.contains("cannot write the profile to"),
        "stderr: {stderr}"
    );
}

/// How far Loam's WebGPU backend may sit from its CPU one: a product summed
/// in another order, and `erf` as WGSL approximates it.
const LOAM_TOLERANCE: f32 = 1e-4;

/// Runs a Loam conformance program, each line of which ends in how far one
/// output sits from the CPU's, and holds it to `lines` lines within tolerance.
fn assert_loam_conformance(program: &str, lines: usize) {
    let program = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../package-loam/conformance")
        .join(program);
    let output = run(&[program.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert_eq!(stdout.lines().count(), lines, "stdout: {stdout}");
    for line in stdout.lines() {
        let deviation: f32 = line
            .rsplit(' ')
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or_else(|| panic!("no deviation in {line:?}"));
        assert!(deviation <= LOAM_TOLERANCE, "{line}");
    }
}

/// Grouped query attention runs on the device, with a past and without, turned
/// in whole or in part, biased, capped and windowed, over one part of positions
/// and over several, along a chain of steps through its cache: appended in
/// place, branched from an earlier past, and moved to a larger cache, and over
/// a prompt whose parts grow to fit a binding.
#[test]
fn loams_webgpu_attention_computes_what_its_cpu_attention_does() {
    assert_loam_conformance("webgpu_attention.wado", 11);
}

/// Graphs built through `Graph` compute on the device what they compute on the
/// CPU: every operation, a gather of consecutive rows as a view, a gather of a
/// mask's rows on the device, a branch either way, attention through a shared
/// cache, and a model's `compute` through the program compiled for each shape
/// class, a second call replaying what the first recorded, products of one
/// input joined into one, a decoder branching on an input along its steps, a
/// counted loop, and a decoding program under each kind of sampler.
#[test]
fn loams_webgpu_graph_computes_what_its_cpu_graph_does() {
    assert_loam_conformance("webgpu_graph.wado", 50);
}

/// A whole model's `compute` stays on the device: each sequence length reads
/// back once, for the logits it returns, and both lengths pad to one capacity,
/// so they run one program in as many dispatches.
#[test]
fn a_model_runs_on_the_device_reading_back_only_what_it_returns() {
    let program = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../package-loam/conformance/webgpu_gpt2.wado");
    let output = run(&[program.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    let dispatches_of = |line: &str| line.split(' ').rev().nth(3).unwrap().to_owned();
    assert_eq!(
        dispatches_of(lines[0]),
        dispatches_of(lines[1]),
        "stdout: {stdout}"
    );
    for line in lines {
        let words: Vec<&str> = line.split(' ').collect();
        assert_eq!(words[words.len() - 3..], ["read", "backs", "1"], "{line}");
        let deviation: f32 = words[words.len() - 6]
            .parse()
            .unwrap_or_else(|_| panic!("no deviation in {line:?}"));
        assert!(deviation <= LOAM_TOLERANCE, "{line}");
    }
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

/// The names of this machine's adapters, in index order, as the refusal of a
/// name nothing matches lists them, so no test assumes which GPU or driver is
/// installed. Every test that calls it also holds that refusal to its wording.
fn adapter_names() -> Vec<String> {
    let output = run(&["--gpu-adapter", "no-such-gpu", "app.wado"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    let (_, listing) = stderr
        .split_once("no GPU adapter matches 'no-such-gpu'. The adapters here:\n")
        .unwrap_or_else(|| panic!("no listing in stderr: {stderr}"));
    listing
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let described = line
                .trim_start()
                .strip_prefix(&format!("{index}: "))
                .unwrap_or_else(|| panic!("no index {index} on: {line}"));
            name_of(described)
        })
        .collect()
}

/// The name an adapter description starts with, which ends where its
/// bracketed details begin.
fn name_of(described: &str) -> String {
    let (name, _) = described
        .split_once(" [")
        .unwrap_or_else(|| panic!("no details on: {described}"));
    name.to_owned()
}

/// Run the compute fixture on the adapter `selector` picks and return the
/// adapter `--log-level info` says the guest got.
fn adapter_selected_by(selector: &str) -> String {
    let output = run(&[
        "--gpu-adapter",
        selector,
        "--log-level",
        "info",
        fixture("compute_double.wado").to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("compute ok"), "stdout: {stdout}");
    let (_, line) = stderr
        .split_once("info: request-adapter: ")
        .unwrap_or_else(|| panic!("no adapter named in stderr: {stderr}"));
    name_of(line.lines().next().unwrap())
}

/// A name selects the adapter it is part of, and only when it is part of one.
/// Which half runs depends on the machine: every name is part of another on
/// one with two GPUs of a model and nothing else.
#[test]
fn a_name_selects_the_one_adapter_it_is_part_of_and_refuses_more() {
    let names = adapter_names();
    let matching = |name: &str| {
        let needle = name.to_lowercase();
        names
            .iter()
            .filter(|other| other.to_lowercase().contains(&needle))
            .count()
    };
    if let Some(name) = names.iter().find(|name| matching(name) == 1) {
        assert_eq!(&adapter_selected_by(&name.to_uppercase()), name);
    } else {
        let output = run(&["--gpu-adapter", &names[0], "app.wado"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("matches more than one GPU adapter; name one by its index"),
            "stderr: {stderr}"
        );
    }
}

/// An index tells apart adapters a name cannot, two of one model among them.
#[test]
fn an_integer_selects_the_adapter_at_that_index() {
    let names = adapter_names();
    let last = names.len() - 1;
    assert_eq!(adapter_selected_by(&last.to_string()), names[last]);
}

#[test]
fn an_index_past_the_last_adapter_is_refused_with_the_adapters_there_are() {
    let count = adapter_names().len();
    let output = run(&["--gpu-adapter", &count.to_string(), "app.wado"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains(&format!("no GPU adapter at index {count}"))
            && stderr.contains("The adapters here:\n  0: "),
        "stderr: {stderr}"
    );
}

/// The runner reads the level itself and hands it to `wado build`, so the
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
fn the_help_names_the_subcommand_and_lists_the_adapters_under_their_indices() {
    let output = run(&["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("wado run-webgpu"), "stdout: {stdout}");
    for (index, name) in adapter_names().iter().enumerate() {
        assert!(
            stdout.contains(&format!("\n  {index}: {name} [")),
            "stdout: {stdout}"
        );
    }
}

/// Help is still help on a machine with nothing to run on.
#[test]
#[cfg(target_os = "linux")]
fn the_help_says_when_there_is_no_adapter() {
    let output = runner()
        .env("VK_DRIVER_FILES", "/nonexistent.json")
        .arg("--help")
        .output()
        .expect("running wado-run-webgpu");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("wado run-webgpu"), "stdout: {stdout}");
    assert!(stdout.contains("no GPU adapter found"), "stdout: {stdout}");
}
