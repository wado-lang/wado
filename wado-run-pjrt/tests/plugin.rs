//! Runs programs on the plugin `WADO_PJRT_PLUGIN` names, through the same
//! `wado-run-pjrt` a real invocation reaches. `mise run test-pjrt` fetches the
//! plugin and sets it, with `WADO`; a plain `cargo test` has no plugin to load.

use std::path::Path;
use std::process::{Command, Output};

/// How far a program may sit from Loam's `Cpu`: a product summed in another
/// order, and `erf` as XLA approximates it.
const LOAM_TOLERANCE: f32 = 1e-4;

/// How the program at `path`, from this crate, ends under `command`.
fn output_of(mut command: Command, path: &str) -> Output {
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .output()
        .expect("wado-run-pjrt runs")
}

/// What the program at `path` prints under `command`.
fn printed_by(command: Command, path: &str) -> String {
    let output = output_of(command, path);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    String::from_utf8(output.stdout).expect("the program prints text")
}

fn run(path: &str) -> String {
    printed_by(Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt")), path)
}

#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn a_program_compiles_and_runs_on_the_device() {
    let printed = run("tests/fixtures/add.wado");
    let mut lines = printed.lines();
    assert_eq!(lines.next(), Some("cpu"));
    // 1.75 and 5.0, little-endian.
    assert_eq!(
        lines.next(),
        Some("[0, 0, 224, 63, 0, 0, 160, 64] as ByteList")
    );
}

/// A newer plugin may stop checking one of these, which then crashes the process
/// here rather than in a program.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn a_plugin_refuses_what_a_guest_gets_wrong() {
    let printed = run("tests/fixtures/malformed.wado");
    assert_eq!(printed.lines().count(), 15, "{printed}");
    assert!(printed.lines().all(|line| line == "true"), "{printed}");
}

#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn an_exit_status_is_the_programs_own() {
    for status in [0, 3] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt"));
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exit.wado"))
            .arg(status.to_string());
        let output = command.output().expect("wado-run-pjrt runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(status), "{stderr}");
        assert!(stderr.is_empty(), "{stderr}");
    }
}

#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn a_program_reaches_what_it_reaches_under_wado_run() {
    const SERVED: &str = "../wado-host/tests/fixtures/served.wado";
    let wado = std::env::var_os("WADO").expect("`mise run test-pjrt` sets WADO");
    let mut under_wado_run = Command::new(wado);
    under_wado_run.arg("run").args(["-D", "ANSWER=42"]);
    let mut under_the_plugin = Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt"));
    under_the_plugin.args(["-D", "ANSWER=42"]);

    assert_eq!(
        printed_by(under_the_plugin, SERVED),
        printed_by(under_wado_run, SERVED)
    );
}

/// An import outside what a runner serves stops the program at its call, as
/// under `wado run`, and not before it starts.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn an_import_nothing_serves_traps_where_it_is_called() {
    let output = output_of(
        Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt")),
        "tests/fixtures/unserved.wado",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "instantiated\n");
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("unknown import"), "{stderr}");
}

/// Every operation a recorded pass holds, the composite decomposed, `compute`
/// over models with a cache and an `If`, one compile per shape class, a counted
/// loop, and a decoding program under each kind of sampler.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn loams_programs_compute_what_its_cpu_graph_does() {
    let printed = run("../package-loam/conformance/pjrt_graph.wado");
    assert_eq!(printed.lines().count(), 54, "{printed}");
    for line in printed.lines() {
        let deviation: f32 = line
            .rsplit(' ')
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or_else(|| panic!("no deviation in {line:?}"));
        assert!(deviation <= LOAM_TOLERANCE, "{line}");
    }
}

/// A freed slot holds another tensor by the time this reads the one it held.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn a_tensor_of_an_ended_pass_is_refused() {
    let output = output_of(
        Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt")),
        "../package-loam/conformance/pjrt_stale.wado",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("is no longer held"), "{stderr}");
}

/// The same programs compiled for a TPU topology, by `libtpu.so`, on a machine
/// with no TPU. `TPU_SKIP_MDS_QUERY` stops it asking a cloud's metadata server
/// where it runs.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn loams_programs_compile_for_a_tpu() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt"));
    command
        .env(
            "WADO_PJRT_PLUGIN",
            std::env::var_os("WADO_PJRT_TPU_PLUGIN").expect("`mise run test-pjrt` sets it"),
        )
        .env("TPU_SKIP_MDS_QUERY", "1");
    let printed = printed_by(command, "../package-loam/conformance/pjrt_topology.wado");
    assert_eq!(printed.lines().count(), 48, "{printed}");
    for line in printed.lines() {
        assert!(line.contains("optimal seconds"), "{line}");
    }
}
