//! End-to-end: `wado check` over a package fails on a Kiln warning wherever
//! the member raising it sits, a test file as much as an entry.

use crate::common::{wado_in, write_kiln_project};
use predicates::prelude::*;

const GREETING_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "hello.wado",
            content: "pub fn hello() -> i32 { return 1; }\n",
            is_entry: true,
        }],
    });
}
"#;

#[test]
fn a_kiln_warning_in_a_test_member_fails_the_package_check() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");
    std::fs::rename(root.join("src/main.wado"), root.join("src/hello_test.wado")).unwrap();
    std::fs::write(
        root.join("src/hello_test.wado"),
        "use { hello } from \"./schema.bin\"\n    with { generator: { module: \"./gen.wado\" } };\n\n\
         test { assert hello() == 1; }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.wado"), "export fn run() {}\n").unwrap();

    wado_in(root).arg("check").assert().success();
    let generated = walkdir(root)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "hello.wado"))
        .expect("the generator wrote hello.wado");
    std::fs::write(&generated, "pub fn hello() -> i32 { return 2; }\n").unwrap();

    wado_in(root)
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("has been modified after generation"));
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}
