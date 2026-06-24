//! End-to-end tests for Kiln path resolution across multiple consumer files.
//!
//! A path written in a `.wado` file (`from`, `generator.inputs`,
//! `generator.output_dir`) must resolve relative to that file — the same rule
//! every other Wado path follows. These tests pin two consequences:
//!
//! 1. A consumer in a subdirectory reads *its own* sibling schema, not a
//!    same-named file at the project root.
//! 2. Two consumers in different directories that both write `from
//!    "./grammar.g4"` get distinct default `output_dir`s instead of clobbering
//!    each other (the original collision the `output_dir` override worked
//!    around).

use std::fs;

mod common;
use common::wado_in;
use predicates::prelude::*;

/// A pass-through generator: it emits the primary schema's contents verbatim as
/// the entry module, so the grammar file *is* the generated Wado source. This
/// makes a cross-read (reading the wrong sibling) observable as a wrong return
/// value.
const PASSTHROUGH_GENERATOR: &str = r#"use { RawRequest, Response, OutputFile, Error } from "core:kiln";

pub struct Options {}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: raw.primary.content,
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
    println(`{v1() + v2()}`);
}
"#,
    )
    .unwrap();
}

#[test]
fn sibling_consumers_resolve_their_own_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    // feat1 reads feat1/grammar.g4 (→1), feat2 reads feat2/grammar.g4 (→2).
    // With the resolution bug both collapse to one invocation reading a single
    // "grammar.g4", so this prints the wrong sum (or fails to find the file).
    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));

    // Distinct default output dirs: one generated entry per consumer.
    let kiln_dir = root.join("build/kiln");
    let out_files: Vec<_> = walk(&kiln_dir)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "out.wado"))
        .collect();
    assert_eq!(
        out_files.len(),
        2,
        "each sibling consumer must get its own generated entry, got {out_files:?}"
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

#[test]
fn bare_relative_paths_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    // A bare (non-`./`) output_dir is rejected: relative paths must be explicit.
    fs::write(
        root.join("src/feat1/mod.wado"),
        "use { hello } from \"./grammar.g4\"\n    with {\n        generator: {\n            module: \"../gen.wado\",\n            output_dir: \"generated\",\n        },\n    };\n\npub fn v1() -> i32 { return hello(); }\n",
    )
    .unwrap();

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be a relative path starting with `./` or `../`"));
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
