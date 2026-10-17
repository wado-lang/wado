//! End-to-end tests for Kiln path resolution across multiple consumer files.
//!
//! A path written in a `.wado` file (`from`, `generator.inputs`,
//! `generator.output_dir`) must resolve relative to that file — the same rule
//! every other Wado path follows. That sibling consumers each read their own
//! same-named schema, and that a bare `output_dir` is refused, are e2e
//! fixtures (`kiln_sibling_consumers_read_their_own_schema`,
//! `kiln_output_dir_must_start_with_dot`). These tests pin what a fixture
//! cannot state: that two consumers naming one schema run it once, and where
//! an explicit `output_dir` writes.

use std::fs;

use crate::common::wado_in;
use predicates::prelude::*;

/// A pass-through generator: it emits the primary schema's contents verbatim as
/// the entry module, so the grammar file *is* the generated Wado source. This
/// makes a cross-read (reading the wrong sibling) observable as a wrong return
/// value. It cannot show a *missing* redirect, though: the loader then parses
/// the grammar as Wado and gets the same result.
const PASSTHROUGH_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: read_text(req.primary.content).unwrap(),
            is_entry: true,
        }],
    });
}
"#;

fn write_project(root: &std::path::Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"multi\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src/feat1")).unwrap();
    fs::create_dir_all(root.join("src/feat2")).unwrap();
    fs::write(root.join("src/gen.wado"), PASSTHROUGH_GENERATOR).unwrap();

    // Two same-named grammars in sibling directories, distinct contents.
    fs::write(
        root.join("src/feat1/grammar.g4"),
        "pub fn hello() -> i32 { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/feat2/grammar.g4"),
        "pub fn hello() -> i32 { return 2; }\n",
    )
    .unwrap();

    let consumer = |val: &str| {
        format!(
            "use {{ hello }} from \"./grammar.g4\"\n    with {{ generator: {{ module: \"../gen.wado\" }} }};\n\npub fn {val}() -> i32 {{ return hello(); }}\n"
        )
    };
    fs::write(root.join("src/feat1/mod.wado"), consumer("v1")).unwrap();
    fs::write(root.join("src/feat2/mod.wado"), consumer("v2")).unwrap();

    fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { v1 } from "./feat1/mod.wado";
use { v2 } from "./feat2/mod.wado";

export fn run() with Stdout {
    println(`${v1() + v2()}`);
}
"#,
    )
    .unwrap();
}

/// Two modules declaring the same clause for the same schema collapse into one
/// invocation, so the generator runs once. Each still imports through it, so
/// each needs its own redirect (WEP 2026-04-12 §"Use-site syntax").
///
/// The schema here is not valid Wado, so a module left unredirected fails to
/// compile rather than parsing its schema as source and passing by accident.
#[test]
fn two_modules_sharing_one_schema_both_redirect() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/gen.wado"),
        r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: `pub fn hello() -> i32 { return ${read_text(req.primary.content).unwrap().trim()}; }`,
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/grammar.g4"), "1\n").unwrap();

    let consumer = |name: &str| {
        format!(
            "use {{ hello }} from \"./grammar.g4\"\n    with {{ generator: {{ module: \"./gen.wado\" }} }};\n\npub fn {name}() -> i32 {{ return hello(); }}\n"
        )
    };
    fs::write(root.join("src/first.wado"), consumer("v1")).unwrap();
    fs::write(root.join("src/second.wado"), consumer("v2")).unwrap();
    fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { v1 } from "./first.wado";
use { v2 } from "./second.wado";

export fn run() with Stdout {
    println(`${v1() + v2()}`);
}
"#,
    )
    .unwrap();

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2"));

    let out_files: Vec<_> = walk(&root.join("build/kiln"))
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "out.wado"))
        .collect();
    assert_eq!(
        out_files.len(),
        1,
        "one schema, one clause: the generator runs once, got {out_files:?}"
    );
}

#[test]
fn explicit_output_dir_is_relative_to_declaring_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    // Give feat1 a `./`-relative output_dir (relative to the declaring file).
    fs::write(
        root.join("src/feat1/mod.wado"),
        "use { hello } from \"./grammar.g4\"\n    with {\n        generator: {\n            module: \"../gen.wado\",\n            output_dir: \"./generated\",\n        },\n    };\n\npub fn v1() -> i32 { return hello(); }\n",
    )
    .unwrap();

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));

    // `./generated` is relative to src/feat1/mod.wado, so it lands next to it.
    assert!(
        root.join("src/feat1/generated/out.wado").exists(),
        "`./`-relative output_dir must resolve relative to the declaring .wado file"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
