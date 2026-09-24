//! End-to-end: a `use ... with { generator: ... }` that no invocation produced
//! a module for reports Kiln's own error, never a parse of the schema as Wado.

use crate::common::{wado_in, write_kiln_project};
use predicates::prelude::*;

/// Emits a file and marks it not the entry, so nothing redirects the `use` site
/// that named this generator.
const NO_ENTRY_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: "pub fn hello() -> i32 { return 1; }",
            is_entry: false,
        }],
    });
}
"#;

#[test]
fn a_generator_that_names_no_entry_reports_kiln_not_a_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "no_entry", NO_ENTRY_GENERATOR, b"99\n");

    wado_in(root)
        .args(["compile", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("kiln: no generated module"))
        .stderr(predicate::str::contains("parse error").not());
}
