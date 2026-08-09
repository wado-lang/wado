//! WIT emission for a Kiln `with { generator }` consumer (issue #1646).
//!
//! The WIT section and `wado wit` text are derived from the WIT subset the main
//! compile retains (`CompileResult::wit_emit_snapshot`, issue #1654), so they
//! reuse the generator redirects and dependency index that compile already ran.
//! There is no second analysis to drift: a consumer whose only path to its
//! import is a generator redirect still ships its `component-type` section,
//! because the section comes from the exact compile that resolved that redirect.

use std::fs;

use crate::common::{custom_sections, wado_in};
use predicates::prelude::*;

/// A generator that emits a fixed valid-Wado parser, ignoring its input. The
/// schema it consumes (`grammar.g4`) is deliberately *not* valid Wado, so the
/// only way the consumer's `use { hello } from "./grammar.g4"` resolves is
/// through the generator redirect. A WIT re-analysis that drops the redirect
/// falls back to loading the raw grammar as Wado, which fails — the exact
/// #1646 failure.
const FIXED_OUTPUT_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.content;
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: "pub fn hello() -> i32 { return 42; }\n",
            is_entry: true,
        }],
    });
}
"#;

/// Not valid Wado (an ANTLR-style grammar stands in for the real `Wado.g4`):
/// loading it as source must fail, so only the generator redirect resolves it.
const NON_WADO_SCHEMA: &str = "grammar Wado;\nprogram : statement* EOF ;\n";

fn write_project(root: &std::path::Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"gen-consumer\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/gen.wado"), FIXED_OUTPUT_GENERATOR).unwrap();
    fs::write(root.join("src/grammar.g4"), NON_WADO_SCHEMA).unwrap();
    fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "./grammar.g4"
    with { generator: { module: "./gen.wado" } };

export fn run() with Stdout {
    println(`${hello()}`);
}
"#,
    )
    .unwrap();
}

#[test]
fn embed_wit_succeeds_for_generator_consumer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);
    let out = root.join("out.wasm");

    wado_in(root)
        .args(["compile", "--embed-wit", "-o"])
        .arg(&out)
        .arg("src/main.wado")
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        sections.iter().any(|(n, _)| n == "component-type"),
        "generator consumer must embed the component-type section, got {sections:?}"
    );
}

#[test]
fn wit_command_emits_for_generator_consumer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    wado_in(root)
        .args(["wit", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("world"));
}

/// Library-world variant of `write_project`: the `[package].lib` entry consumes
/// the same generator. Exercises the `--lib` embedding path, which #1646 also
/// broke (`build/lib.wasm`) and which flows through different code than the
/// command world.
fn write_lib_project(root: &std::path::Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"gen-lib\"\nversion = \"0.1.0\"\n\
         lib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/gen.wado"), FIXED_OUTPUT_GENERATOR).unwrap();
    fs::write(root.join("src/grammar.g4"), NON_WADO_SCHEMA).unwrap();
    fs::write(
        root.join("src/lib.wado"),
        r#"use { hello as hello_impl } from "./grammar.g4"
    with { generator: { module: "./gen.wado" } };

export fn hello() -> i32 { return hello_impl(); }
"#,
    )
    .unwrap();
}

#[test]
fn build_lib_embeds_component_type_for_generator_consumer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_lib_project(root);
    let out = root.join("out.wasm");

    wado_in(root)
        .args(["build", "--lib", "-o"])
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        sections.iter().any(|(n, _)| n == "component-type"),
        "generator-consumer library build must embed the component-type section, got {sections:?}"
    );
}

#[test]
fn wit_lib_emits_for_generator_consumer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_lib_project(root);

    wado_in(root)
        .args(["wit", "--lib"])
        .assert()
        .success()
        .stdout(predicate::str::contains("world"));
}
