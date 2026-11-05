//! Runs programs on the plugin `WADO_PJRT_PLUGIN` names, through the same
//! `wado-run-pjrt` a real invocation reaches. `mise run test-pjrt` fetches the
//! plugin and sets it, with `WADO`; a plain `cargo test` has no plugin to load.

use std::path::Path;
use std::process::Command;

/// How far a program may sit from Loam's `Cpu`: a product summed in another
/// order, and `erf` as XLA approximates it.
const LOAM_TOLERANCE: f32 = 1e-4;

/// What the program at `path`, from this crate, prints.
fn run(path: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .output()
        .expect("wado-run-pjrt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    String::from_utf8(output.stdout).expect("the program prints text")
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

/// Every operation a recorded pass holds, the composite decomposed, and
/// `compute` over models with a cache and an `If`, one compile per shape class.
#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn loams_programs_compute_what_its_cpu_graph_does() {
    let printed = run("../package-loam/conformance/pjrt_graph.wado");
    assert_eq!(printed.lines().count(), 26, "{printed}");
    for line in printed.lines() {
        let deviation: f32 = line
            .rsplit(' ')
            .next()
            .and_then(|d| d.parse().ok())
            .unwrap_or_else(|| panic!("no deviation in {line:?}"));
        assert!(deviation <= LOAM_TOLERANCE, "{line}");
    }
}
