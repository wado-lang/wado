//! End-to-end: an options diagnostic points at the offending key.
//!
//! The typed check runs in the driver, long after the AST is gone, so the
//! spans it reports come from the key spans the parser recorded on the
//! `with { generator: { options: … } }` clause.

use predicates::prelude::*;
use std::fs;

use crate::common::wado_in;

/// Write a generator package declaring one required `verbose` option, and a
/// consumer whose `options` table is `options_table`, on line 5 of
/// `src/main.wado`.
fn write_project(root: &std::path::Path, options_table: &str) -> std::path::PathBuf {
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
            content: "pub fn greeting() -> String { return \"hi\"; }",
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();

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
"lib:gen" = { path = "../gen" }
"#,
    )
    .unwrap();
    fs::write(app.join("src/schema.idl"), "anything\n").unwrap();
    fs::write(
        app.join("src/main.wado"),
        format!(
            r#"use {{ println, Stdout }} from "core:cli";
use {{ greeting }} from "./schema.idl" with {{
    generator: {{
        module: "lib:gen",
        options: {options_table},
    }},
}};

export fn run() with Stdout {{
    println(greeting());
}}
"#
        ),
    )
    .unwrap();
    app
}

/// A misspelled key squiggles the key itself (line 5, column 20), and the
/// required field it failed to spell falls back to the `options:` key that
/// owns the table (line 5, column 9) — neither lands on line 1.
#[test]
fn options_diagnostics_point_at_the_offending_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), "{ verbsoe: false }");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "main.wado:5:20: error: kiln: unknown options field `options.verbsoe`",
        ))
        .stderr(predicate::str::contains(
            "main.wado:5:9: error: kiln: required options field `options.verbose`",
        ));
}

/// A value of the wrong type squiggles its own key, not the table's.
#[test]
fn type_mismatch_points_at_the_field_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), "{ verbose: 1 }");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "main.wado:5:20: error: kiln: `options.verbose` expected bool, got integer",
        ));
}
