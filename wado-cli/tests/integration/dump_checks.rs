//! `wado dump` runs the checks `wado compile` runs before synthesis, with the
//! same inputs, so it reports what they reject and dumps what they accept.

use std::fs;

use crate::common::{project_root, wado_in};
use predicates::prelude::*;

#[test]
fn dump_rejects_what_compile_rejects() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("entry.wado"),
        "use { Fields } from \"wasi:http\";\n\n\
         global mut SLOT: Option<Fields> = null;\n\n\
         fn eat(_f: Option<Fields>) {\n}\n\n\
         export fn run() {\n    eat(SLOT);\n}\n",
    )
    .unwrap();

    let rejection = "cannot read global `SLOT` by value";
    wado_in(root)
        .args(["compile", "entry.wado", "-o", "out.wasm"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(rejection));
    wado_in(root)
        .args(["dump", "--tir-monomorphized", "entry.wado"])
        .assert()
        .stderr(
            predicate::str::contains(rejection).and(predicate::str::contains("panicked").not()),
        );
}

/// A provider discharges the guest effect its dependency imports. `dump`
/// builds no provider, yet must not demand that effect of the caller.
#[test]
fn dump_accepts_what_a_provider_satisfies() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sub = project_root().join("wado-compiler/tests/fixtures/sub");
    fs::create_dir(root.join("sub")).unwrap();
    for file in ["hlc.wasm", "hl_ext.wado"] {
        fs::copy(sub.join(file), root.join("sub").join(file)).unwrap();
    }
    fs::write(
        root.join("entry.wado"),
        "use { println, Stdout } from \"core:cli\";\n\
         use { Hlc } from \"./sub/hlc.wasm\" with { type: \"wasm\", provider: \"./sub/hl_ext.wado\" };\n\n\
         export fn run() with Stdout {\n    println(Hlc::wrap(\"x\", \"wado\").text);\n}\n",
    )
    .unwrap();

    wado_in(root)
        .args(["dump", "--tir-monomorphized", "entry.wado"])
        .assert()
        .success()
        .stderr(predicate::str::contains("missing effect").not())
        .stdout(predicate::str::contains("fn "));
}
