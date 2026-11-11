//! `wado dump` runs the pipeline `wado compile` runs, with the same inputs: it
//! fails on what compile rejects, after printing the views built before the
//! failure, and dumps what it accepts.

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
        .args(["dump", "--ast", "entry.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("fn eat"))
        .stderr(
            predicate::str::contains(rejection).and(predicate::str::contains("panicked").not()),
        );
}

#[test]
fn dump_fails_on_a_type_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("entry.wado"),
        "export fn run() {\n    let _x: i32 = \"s\";\n}\n",
    )
    .unwrap();
    wado_in(root)
        .args(["dump", "--tir-monomorphized", "entry.wado"])
        .assert()
        .failure();
}

/// A program rejected past the checks, where a substitution lands, still
/// prints the views built before the failure.
#[test]
fn dump_prints_the_views_before_a_later_rejection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("entry.wado"),
        "fn name_of<T>() -> String {\n    return Reflect::<T>::decl_name();\n}\n\n\
         export fn run() {\n    let _n = name_of::<i32>();\n}\n",
    )
    .unwrap();
    wado_in(root)
        .args(["dump", "--tir-resolved", "entry.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("fn name_of"))
        .stderr(predicate::str::contains(
            "type `i32` does not implement trait `Reflect`",
        ));
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
