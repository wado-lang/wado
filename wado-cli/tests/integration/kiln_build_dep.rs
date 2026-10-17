//! End-to-end test: a Kiln generator referenced by a `lib:` nickname
//! `[build-dependencies]` specifier (`module: "lib:gen"`) instead of a relative
//! path.
//!
//! The specifier resolves against `[build-dependencies]` and dispatches on the
//! entry's source: a path entry reads the dependency package's
//! `[world]."core:kiln/generator"` entry, compiles it, runs it, and the
//! generated module is imported by the consumer.

use predicates::prelude::*;
use std::fs;

use crate::common::wado_in;

#[test]
fn generator_resolved_by_build_dependency_nickname() {
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

export struct Options {
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

    // Consumer: references the generator by a `lib:` nickname build-dependency.
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
        r#"use { println, Stdout } from "core:cli";
use { greeting } from "./schema.idl" with {
    generator: {
        module: "lib:gen",
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

    // Run from below the package, whose own directory holds no `wado.toml`:
    // the package is found above the working directory, and the recorded
    // paths stay package-relative, as they are from the package root.
    fs::remove_dir_all(app.join("build")).unwrap();
    wado_in(&app.join("src"))
        .args(["check", "main.wado"])
        .assert()
        .success();
    let kiln_dir = app.join("build/kiln");
    let metadata = fs::read_dir(&kiln_dir)
        .unwrap()
        .map(|e| e.unwrap().path().join("schema.idl.kiln.json"))
        .find(|p| p.is_file())
        .expect("the invocation's metadata is written under the package root");
    let metadata = fs::read_to_string(metadata).unwrap();
    assert!(
        metadata.contains("\"path\": \"src/schema.idl\""),
        "primary is recorded package-relative:\n{metadata}"
    );
}

/// A path `[build-dependencies]` generator whose package is a **workspace
/// member** (so its `[package]` fields inherit from `[workspace.package]`),
/// invoked with a **bare relative entry path** (`src/main.wado`) from the
/// consumer directory.
///
/// This is the shape of the real `gale-highlight-wado` package. A bare relative
/// entry makes manifest discovery return an empty/relative project root; the
/// generator package's `wado.toml` is then resolved through a relative
/// `../gen` path that cannot locate the workspace root, so member inheritance
/// fails, the generator is not rewritten to a `LocalPath`, and the pipeline
/// reports it as an unsupported (registry-only) spec — "generator execution is
/// not available in this host". The manifest root must be absolute so the
/// dependency package resolves like it does under an absolute entry path.
#[test]
fn generator_build_dependency_in_workspace_member_bare_relative_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Workspace: members inherit version + namespace.
    fs::write(
        root.join("wado-workspace.toml"),
        r#"[workspace]
members = ["gen", "app"]

[workspace.package]
version = "0.1.0"
namespace = "acme"
"#,
    )
    .unwrap();

    // Generator package (workspace member: no explicit version/namespace).
    let gen_pkg = root.join("gen");
    fs::create_dir_all(gen_pkg.join("src")).unwrap();
    fs::write(
        gen_pkg.join("wado.toml"),
        r#"[package]
name = "gen"

[world]
"core:kiln/generator" = "src/generator.wado"
"#,
    )
    .unwrap();
    fs::write(
        gen_pkg.join("src/generator.wado"),
        r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.content;
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

    // Consumer package (workspace member) referencing the generator by nickname.
    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        r#"[package]
name = "app"

[world]
"wasi:cli/command" = "src/main.wado"

[build-dependencies]
"lib:gen" = { path = "../gen", package = "acme:gen", version = "^0.1.0" }
"#,
    )
    .unwrap();
    fs::write(app.join("src/schema.idl"), "anything\n").unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { greeting } from "./schema.idl" with {
    generator: { module: "lib:gen" },
};

export fn run() with Stdout {
    println(greeting());
}
"#,
    )
    .unwrap();

    // Bare relative entry path from the consumer directory.
    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from generator"));
}

/// A clause in a source dependency's module resolves its generator through that
/// dependency's own `[build-dependencies]`, which the consumer does not declare,
/// and writes into the consumer's tree rather than the dependency's.
#[test]
fn a_dependency_clause_resolves_through_the_dependency_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let gen_pkg = root.join("gen");
    fs::create_dir_all(gen_pkg.join("src")).unwrap();
    fs::write(
        gen_pkg.join("wado.toml"),
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"core:kiln/generator\" = \"src/generator.wado\"\n",
    )
    .unwrap();
    fs::write(
        gen_pkg.join("src/generator.wado"),
        r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: "pub fn greeting() -> String { return \"hi from a dependency\"; }",
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();

    let lib = root.join("lib");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n\n\
         [build-dependencies]\n\"lib:gen\" = { path = \"../gen\" }\n",
    )
    .unwrap();
    fs::write(lib.join("src/schema.idl"), "anything\n").unwrap();
    fs::write(
        lib.join("src/lib.wado"),
        r#"use { greeting } from "./schema.idl" with { generator: { module: "lib:gen" } };

pub fn hello() -> String {
    return greeting();
}
"#,
    )
    .unwrap();

    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n\n\
         [dependencies]\n\"lib:lib\" = { path = \"../lib\" }\n",
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "lib:lib";

export fn run() with Stdout {
    println(hello());
}
"#,
    )
    .unwrap();

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from a dependency"));
    assert!(
        app.join("build/kiln").is_dir(),
        "the consumer's tree holds the output"
    );
    assert!(
        !lib.join("build").exists(),
        "the dependency's tree is not written"
    );
}
