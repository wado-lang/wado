//! Runs programs on the plugin `WADO_PJRT_PLUGIN` names, through the same
//! `wado-run-pjrt` a real invocation reaches. `mise run test-pjrt` fetches the
//! plugin and sets it, with `WADO`; a plain `cargo test` has no plugin to load.

use std::path::Path;
use std::process::Command;

/// What the program prints.
fn run(fixture: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wado-run-pjrt"))
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
        )
        .output()
        .expect("wado-run-pjrt runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    String::from_utf8(output.stdout).expect("the program prints text")
}

#[test]
#[ignore = "needs a plugin: run `mise run test-pjrt`"]
fn a_program_compiles_and_runs_on_the_device() {
    let printed = run("add.wado");
    let mut lines = printed.lines();
    assert_eq!(lines.next(), Some("cpu"));
    // 1.75 and 5.0, little-endian.
    assert_eq!(
        lines.next(),
        Some("[0, 0, 224, 63, 0, 0, 160, 64] as ByteList")
    );
}
