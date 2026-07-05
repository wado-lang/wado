//! End-to-end test: a Kiln generator referenced by its `[build-dependencies]`
//! name (`module: "gen"`) instead of a relative path.
//!
//! The bare name resolves against `[build-dependencies]`; the provider reads
//! the dependency package's `[world]."core:kiln/generator"` entry, compiles
//! it, runs it, and the generated module is imported by the consumer.

use predicates::prelude::*;
use std::fs;

mod common;
use common::wado_in;

#[test]
fn generator_resolved_by_build_dependency_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Generator package: declares the `core:kiln/generator` world entry.
    let gen_pkg = root.join("gen");
    fs::create_dir_all(gen_pkg.join("src")).unwrap();
    fs::write(
        gen_pkg.join("wado.toml"),
        r#"[package]
name = "gen"
version = "0.1.0"

[world]
"core:kiln/generator" = "src/generator.wado"
"#,
    )
    .unwrap();
    fs::write(
        gen_pkg.join("src/generator.wado"),
        r#"use { Request, Response, OutputFile, Error } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.options.verbose;
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: "pub fn greeting() -> String { return \"hi from generator\"; }",
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();

    // Consumer: references the generator by its build-dependency name.
    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        r#"[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"

[build-dependencies]
gen = { path = "../gen" }
"#,
    )
    .unwrap();
    fs::write(app.join("src/schema.idl"), "anything\n").unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { greeting } from "./schema.idl" with {
    generator: {
        module: "gen",
        options: { verbose: false },
    },
};

export fn run() with Stdout {
    println(greeting());
}
"#,
    )
    .unwrap();

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from generator"));

    // `wado check` resolves the build-dependency generator too.
    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .success();
}
