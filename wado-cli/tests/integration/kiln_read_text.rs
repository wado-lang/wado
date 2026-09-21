//! End-to-end tests for `core:kiln`'s stream readers: `read_all` over an input
//! that is not text, and `read_text` reporting a malformed one as a diagnostic.

use crate::common::{wado_in, write_kiln_project};
use predicates::prelude::*;

const TEXT_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let text = match read_text(req.primary.content) {
        Ok(text) => text,
        Err(e) => { return Result::Err(Error::InvalidSchema(`schema is not text: ${e}`)); },
    };
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: `pub fn hello() -> i32 { return ${text.trim()}; }`,
            is_entry: true,
        }],
    });
}
"#;

const BYTES_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error, read_all } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let mut sum = 0;
    for let b of read_all(req.primary.content) as List<u8> {
        sum += b as i32;
    }
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: `pub fn hello() -> i32 { return ${sum}; }`,
            is_entry: true,
        }],
    });
}
"#;

#[test]
fn a_malformed_input_surfaces_as_the_generators_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 0xE6 leads a three-byte sequence that 0x41 cannot continue.
    write_kiln_project(root, "texted", TEXT_GENERATOR, &[b'7', 0xE6, 0x41, b'\n']);

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("schema is not text"));
}

#[test]
fn read_all_hands_a_generator_an_input_that_is_not_text() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 0xFF and 0x80 can appear in no UTF-8 sequence, so this reaches the
    // generator only if the boundary carries bytes rather than a string.
    write_kiln_project(root, "texted", BYTES_GENERATOR, &[0x00, 0xFF, 0x80, 0x41]);

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("448"));
}
