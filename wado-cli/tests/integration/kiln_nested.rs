//! A generator that uses a generator, end to end: `wado run` over a project
//! whose generator's own source imports a module Kiln produces.
//!
//! See WEP 2026-04-12 §"A generator may use a generator".

use std::path::{Path, PathBuf};

use crate::common::wado_in;

/// The inner generator: turns its input's text into a function answering it.
const INNER_GENERATOR: &str = r#"
use { Request, Response, Error, OutputFile, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let text = match read_text(req.primary.content) {
        Ok(t) => t,
        Err(e) => {
            return Result::Err(Error::InvalidSchema(`${e}`));
        },
    };
    let files: List<OutputFile> = [
        OutputFile {
            path: "value.wado",
            content: `pub fn answer() -> i32 { return ${text.trim()}; }\n`,
            is_entry: true,
        },
    ];
    return Result::Ok(Response { files });
}
"#;

/// The outer generator: its own source imports what the inner one produces,
/// and what it emits carries that value out to the program.
const OUTER_GENERATOR: &str = r#"
use { Request, Response, Error, OutputFile } from "core:kiln";
use { answer } from "./value.txt"
    with {
        generator: { module: "./inner.wado" },
    };

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    let files: List<OutputFile> = [
        OutputFile {
            path: "model.wado",
            content: `pub fn model() -> i32 { return ${answer()}; }\n`,
            is_entry: true,
        },
    ];
    return Result::Ok(Response { files });
}
"#;

const MAIN: &str = r#"use { println, Stdout } from "core:cli";
use { model } from "./model.spec"
    with { generator: { module: "./outer.wado" } };

export fn run() with Stdout {
    println(`model ${model()}`);
}
"#;

fn unique_tmp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wado-{label}-{}", std::process::id()))
}

/// A project whose generator is itself built from a generated module. `value`
/// is what the innermost input says, and it reaches the program's output.
fn nested_project(label: &str, value: &str) -> PathBuf {
    let root = unique_tmp(label);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/inner.wado"), INNER_GENERATOR).unwrap();
    std::fs::write(root.join("src/outer.wado"), OUTER_GENERATOR).unwrap();
    std::fs::write(root.join("src/value.txt"), value).unwrap();
    std::fs::write(root.join("src/model.spec"), "a model\n").unwrap();
    std::fs::write(root.join("src/main.wado"), MAIN).unwrap();
    root
}

fn run_project(root: &Path) -> std::process::Output {
    wado_in(root)
        .args(["run", "src/main.wado"])
        .output()
        .expect("wado run")
}

#[test]
fn a_generator_importing_a_generated_module_runs() {
    let root = nested_project("kiln-nested-run", "42\n");

    let out = run_project(&root);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a generator whose source imports a generated module must build:\n{}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 42"),
        "the innermost input must reach the program's output, got: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn editing_the_innermost_input_rebuilds_the_generator() {
    let root = nested_project("kiln-nested-rebuild", "42\n");
    assert!(run_project(&root).status.success(), "first build");

    // The generated module is an input to the outer generator, so what it was
    // generated from reaches the outer generator's identity.
    std::fs::write(root.join("src/value.txt"), "7\n").unwrap();

    let out = run_project(&root);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "rebuild after editing the innermost input:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 7"),
        "editing what the inner generator reads must rebuild the outer one, got: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A generator whose own source is produced by an invocation that reaches back
/// to it: there is no order to run the two in.
const CYCLIC_GENERATOR: &str = r#"
use { Request, Response, Error, OutputFile } from "core:kiln";
use { answer } from "./loop.spec"
    with {
        generator: { module: "./cyclic.wado" },
    };

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    let files: List<OutputFile> = [
        OutputFile {
            path: "model.wado",
            content: `pub fn model() -> i32 { return ${answer()}; }\n`,
            is_entry: true,
        },
    ];
    return Result::Ok(Response { files });
}
"#;

#[test]
fn a_generator_cycle_is_reported_naming_the_invocations() {
    let root = nested_project("kiln-nested-cycle", "42\n");
    std::fs::write(root.join("src/cyclic.wado"), CYCLIC_GENERATOR).unwrap();
    std::fs::write(root.join("src/loop.spec"), "a model\n").unwrap();
    std::fs::write(
        root.join("src/main.wado"),
        MAIN.replace("./outer.wado", "./cyclic.wado"),
    )
    .unwrap();

    let out = run_project(&root);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a generator cycle must stop the build"
    );
    assert!(
        stderr.contains("cycle") && stderr.contains("cyclic.wado"),
        "the cycle must be reported naming the generator in it, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
