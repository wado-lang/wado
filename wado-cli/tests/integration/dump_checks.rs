//! `wado dump` runs the checks `wado compile` runs before synthesis, and
//! reports a program they reject as `compile` does, rather than handing it to
//! the phases after them.

use std::fs;

use crate::common::wado_in;
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
        .stderr(predicate::str::contains(rejection).and(predicate::str::contains("panicked").not()));
}
