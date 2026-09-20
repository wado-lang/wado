//! End-to-end test for `core:kiln`'s `read_text` against a malformed input:
//! the decode failure reaches the compiler as the generator's own diagnostic.

use std::fs;

use crate::common::wado_in;
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

fn write_project(root: &std::path::Path, schema: &[u8]) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"texted\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/gen.wado"), TEXT_GENERATOR).unwrap();
    fs::write(root.join("src/schema.bin"), schema).unwrap();
    fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "./schema.bin"
    with { generator: { module: "./gen.wado" } };

export fn run() with Stdout {
    println(`${hello()}`);
}
"#,
    )
    .unwrap();
}

#[test]
fn a_malformed_input_surfaces_as_the_generators_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 0xE6 leads a three-byte sequence that 0x41 cannot continue.
    write_project(root, &[b'7', 0xE6, 0x41, b'\n']);

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("schema is not text"));
}
