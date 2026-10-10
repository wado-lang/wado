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

/// A misspelled key stops `run` before the generator runs, whether or not the
/// generator declares any options: it never runs on options its use site did
/// not write, and the program is never built from what it would produce.
#[test]
fn unknown_option_stops_the_run_before_the_generator() {
    let no_options = VERBOSE_GENERATOR
        .replace("pub struct Options {\n    pub verbose: bool,\n}\n", "")
        .replace("Request<Options>", "Request")
        .replace("    let _ = req.options.verbose;\n", "");
    for generator in [VERBOSE_GENERATOR, no_options.as_str()] {
        let tmp = tempfile::tempdir().unwrap();
        let app = write_project(tmp.path(), generator, "{ verbose: false, verbsoe: false }");

        wado_in(&app)
            .args(["run", "src/main.wado"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "main.wado:5:36: error: kiln: unknown options field `options.verbsoe`",
            ))
            .stderr(predicate::str::contains("generator host error").not())
            .stdout("");
    }
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

/// An entry module may re-export `Options` from the module that declares it,
/// which the generator's other entry points then share.
#[test]
fn reexported_options_reach_the_generator() {
    let tmp = tempfile::tempdir().unwrap();
    let (declaration, rest) = MAP_GENERATOR
        .split_once("pub struct Options {\n    pub sizes: TreeMap<String, i32>,\n}\n")
        .unwrap();
    let generator = format!("{declaration}pub use {{ Options }} from \"./options.wado\";\n{rest}");
    let app = write_project(tmp.path(), &generator, "{ sizes: { small: 1, large: 3 } }");
    fs::write(
        tmp.path().join("gen/src/options.wado"),
        "use { TreeMap } from \"core:collections\";\n\npub struct Options {\n    pub sizes: TreeMap<String, i32>,\n}\n",
    )
    .unwrap();

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("large=3,small=1\n");
}

/// A list or map field may default to a literal, which a use site that leaves
/// it out receives.
#[test]
fn list_and_map_defaults_reach_the_generator() {
    let tmp = tempfile::tempdir().unwrap();
    let generator = MAP_GENERATOR
        .replace(
            "pub sizes: TreeMap<String, i32>,",
            "pub sizes: TreeMap<String, i32> = { small: 1, large: 3 },\n    pub tags: List<String> = [\"a\", \"b\"],",
        )
        .replace("spelled.join(\",\")", "spelled.join(\",\")};${req.options.tags.join(\",\")");
    let app = write_project(tmp.path(), &generator, "{}");

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("large=3,small=1;a,b\n");
}

/// A repeated key in a map default keeps its last value, as the `TreeMap` the
/// pairs build does. A `{ k: v }` literal refuses one, and spelling the `from`
/// call it lowers to does not.
#[test]
fn a_repeated_key_in_a_map_default_keeps_its_last_value() {
    let tmp = tempfile::tempdir().unwrap();
    let generator = MAP_GENERATOR.replace(
        "pub sizes: TreeMap<String, i32>,",
        "pub sizes: TreeMap<String, i32> = TreeMap::from([[\"small\", 1], [\"small\", 2]]),",
    );
    let app = write_project(tmp.path(), &generator, "{}");

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("small=2\n");
}

/// A default a call computes is not a literal, even one taking a literal.
#[test]
fn a_computed_list_default_stops_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let generator = MAP_GENERATOR
        .replace(
            "pub sizes: TreeMap<String, i32>,",
            "pub sizes: TreeMap<String, i32>,\n    pub tags: List<String> = twice([\"a\"]),",
        )
        .replace(
            "export fn generate",
            "fn twice(l: List<String>) -> List<String> {\n    let mut out = l;\n    out.extend(&l);\n    return out;\n}\n\nexport fn generate",
        );
    let app = write_project(tmp.path(), &generator, "{}");

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "default value must be a literal matching the declared type",
        ));
}

/// A re-export that narrows `Options` below `pub` is named as the cause.
#[test]
fn a_narrowing_reexport_of_options_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (declaration, rest) = MAP_GENERATOR
        .split_once("pub struct Options {\n    pub sizes: TreeMap<String, i32>,\n}\n")
        .unwrap();
    let generator =
        format!("{declaration}internal use {{ Options }} from \"./options.wado\";\n{rest}");
    let app = write_project(tmp.path(), &generator, "{}");
    fs::write(
        tmp.path().join("gen/src/options.wado"),
        "use { TreeMap } from \"core:collections\";\n\npub struct Options {\n    pub sizes: TreeMap<String, i32>,\n}\n",
    )
    .unwrap();

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "kiln: `Options` must be `pub`, and so must a `use` re-exporting it",
        ));
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
