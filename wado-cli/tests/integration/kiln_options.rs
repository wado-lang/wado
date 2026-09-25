//! End-to-end: an options table reaches the generator, and a diagnostic about
//! one points at the offending key, from the driver under `check` and from the
//! language service under `query`.

use predicates::prelude::*;
use std::fs;

use crate::common::wado_in;

/// A generator declaring one required `verbose` option.
const VERBOSE_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

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
"#;

/// A generator whose `greeting` spells out the map option it was given.
const MAP_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";
use { TreeMap } from "core:collections";

pub struct Options {
    pub sizes: TreeMap<String, i32>,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let mut spelled: List<String> = [];
    for let [k, v] of req.options.sizes.entries() {
        spelled.push(`${k}=${v}`);
    }
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: `pub fn greeting() -> String { return "${spelled.join(",")}"; }`,
            is_entry: true,
        }],
    });
}
"#;

/// Write the generator package `generator` and a consumer whose `options`
/// table is `options_table`, on line 5 of `src/main.wado`.
fn write_project(
    root: &std::path::Path,
    generator: &str,
    options_table: &str,
) -> std::path::PathBuf {
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
    fs::write(gen_pkg.join("src/generator.wado"), generator).unwrap();

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

/// A misspelled key squiggles the key itself (line 5, column 20). The required
/// field it failed to spell falls back to the `options:` key that owns the
/// table (line 5, column 9). Neither lands on line 1.
#[test]
fn options_diagnostics_point_at_the_offending_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), VERBOSE_GENERATOR, "{ verbsoe: false }");

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

/// The language service reports the same key, on the same source, with no
/// generated output on disk and no generator run.
#[test]
fn query_diagnostics_points_at_the_offending_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), VERBOSE_GENERATOR, "{ verbsoe: false }");

    wado_in(&app)
        .args(["query", "diagnostics", "src/main.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "src/main.wado:5:20: error: kiln: unknown options field `options.verbsoe`",
        ));
}

/// A value of the wrong type squiggles its own key, not the table's.
#[test]
fn type_mismatch_points_at_the_field_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), VERBOSE_GENERATOR, "{ verbose: 1 }");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "main.wado:5:20: error: kiln: `options.verbose` expected bool, got integer",
        ));
}

/// A `TreeMap<String, V>` option takes an object whose keys are the author's
/// own, and the generator reads them in key order.
#[test]
fn map_option_reaches_the_generator() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(
        tmp.path(),
        MAP_GENERATOR,
        "{ sizes: { small: 1, large: 3 } }",
    );

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("large=3,small=1\n");
}

/// An `Options` field no options table can express stops the generator's own
/// compile, so the generator is never run with the field left out.
#[test]
fn unsupported_option_type_stops_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let generator = VERBOSE_GENERATOR.replace("pub verbose: bool", "pub verbose: [bool, bool]");
    let app = write_project(tmp.path(), &generator, "{}");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "is not supported in generator options",
        ))
        .stderr(predicate::str::contains("missing required options field").not());
}

/// A map value of the wrong type squiggles the key it sits under.
#[test]
fn map_value_mismatch_points_at_its_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = write_project(tmp.path(), MAP_GENERATOR, "{ sizes: { small: true } }");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "main.wado:5:29: error: kiln: `options.sizes.small` expected i32, got bool",
        ));
}
