//! End-to-end: the language service points an options diagnostic at the
//! offending key, as the driver does. The driver's diagnostics are e2e
//! fixtures (`kiln_options_*`).

use predicates::prelude::*;
use std::fs;

use crate::common::wado_in;

/// A generator declaring one required `verbose` option.
const VERBOSE_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export struct Options {
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
"#;

/// Write the generator package and a consumer whose `options` table is
/// `options_table`, on line 5 of `src/main.wado`.
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
    fs::write(gen_pkg.join("src/generator.wado"), VERBOSE_GENERATOR).unwrap();

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

/// The language service reports the misspelled key (line 5, column 20), with
/// no generated output on disk and no generator run.
#[test]
fn query_diagnostics_points_at_the_offending_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), "{ verbsoe: false }");

    wado_in(&app)
        .args(["query", "diagnostics", "src/main.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "src/main.wado:5:20: error: kiln: unknown options field `options.verbsoe`",
        ));
}
