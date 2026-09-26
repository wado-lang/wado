//! End-to-end: a generator that traps reports the trap, not only where it was.

use crate::common::{wado_in, write_kiln_project};
use predicates::prelude::*;

const PANICKING_GENERATOR: &str = r#"use { Request, Response, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    panic("the schema names no table");
}
"#;

#[test]
fn a_generator_that_traps_names_the_trap() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "panicking", PANICKING_GENERATOR, b"99\n");

    wado_in(root)
        .args(["compile", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wasm trap"));
}
