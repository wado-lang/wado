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
use std::path::Path;

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
            content: `${req.origin().header()}pub fn greeting() -> String { return "hi from generator"; }`,
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
    let origin = req.origin();
    let _ = req.primary.content;
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: `${origin.header()}pub fn greeting() -> String { return "hi from generator"; }`,
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
            content: `${req.origin().header()}use { leaf } from "lib:leaf";\npub fn greeting() -> String { return leaf(); }`,
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();

    // The generated module imports what the dependency declares, not the consumer.
    let leaf = root.join("leaf");
    fs::create_dir_all(leaf.join("src")).unwrap();
    fs::write(
        leaf.join("wado.toml"),
        "[package]\nname = \"leaf\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        leaf.join("src/lib.wado"),
        "pub fn leaf() -> String { return \"hi from a dependency\"; }\n",
    )
    .unwrap();

    let lib = root.join("lib");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n\n\
         [dependencies]\n\"lib:leaf\" = { path = \"../leaf\" }\n\n\
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

    // Reached by a path rather than by its specifier, the file is still the
    // dependency's, and so is its clause.
    fs::remove_dir_all(app.join("build")).unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "../../lib/src/lib.wado";

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
        !lib.join("build").exists(),
        "the dependency's tree is not written"
    );

    // Undeclared, the package still resolves its clause through its own manifest.
    fs::remove_dir_all(app.join("build")).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from a dependency"));
    assert!(
        !lib.join("build").exists(),
        "the package's tree is not written"
    );
}

/// A generator package at `dir` whose one output says `greeting`.
pub(crate) fn greeting_generator(dir: &Path, greeting: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    fs::write(
        dir.join("wado.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"core:kiln/generator\" = \"src/generator.wado\"\n"
        ),
    )
    .unwrap();
    fs::write(
        dir.join("src/generator.wado"),
        format!(
            r#"use {{ Request, Response, OutputFile, Error }} from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {{
    return Result::Ok(Response {{
        files: [OutputFile {{
            path: "greeting.wado",
            content: `${{req.origin().header()}}pub fn greeting() -> String {{ return "{greeting}"; }}`,
            is_entry: true,
        }}],
    }});
}}
"#
        ),
    )
    .unwrap();
}

/// A clause names a file a dependency exports as its primary and as an input.
/// The generator, the header and the cache record see the file by its package
/// coordinate, never by where it sits on this machine, and an unlisted file is
/// refused at the package.
#[test]
fn a_clause_generates_from_files_a_dependency_exports() {
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
        r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> with Stream {
    let origin = req.origin();
    let mut text = read_text(req.primary.content).unwrap();
    for let input of req.inputs {
        text.push_str(&read_text(input.content).unwrap());
    }
    return Result::Ok(Response {
        files: [OutputFile { path: "out.wado", content: `${origin.header()}${text}`, is_entry: true }],
    });
}
"#,
    )
    .unwrap();

    let schemas = root.join("schemas");
    fs::create_dir_all(schemas.join("assets")).unwrap();
    fs::write(
        schemas.join("wado.toml"),
        "[package]\nname = \"schemas\"\nnamespace = \"acme\"\nversion = \"0.2.0\"\n\
         lib = \"lib.wado\"\nexports = [\"assets/a.idl\", \"assets/b.idl\"]\n",
    )
    .unwrap();
    fs::write(schemas.join("lib.wado"), "").unwrap();
    fs::write(
        schemas.join("assets/a.idl"),
        "pub fn a() -> i32 { return 1; }\n",
    )
    .unwrap();
    fs::write(
        schemas.join("assets/b.idl"),
        "pub fn b() -> i32 { return 2; }\n",
    )
    .unwrap();
    fs::write(schemas.join("assets/hidden.idl"), "\n").unwrap();
    // A package nested where the files lie does not rename them.
    fs::write(
        schemas.join("assets/wado.toml"),
        "[package]\nname = \"nested\"\nnamespace = \"acme\"\nversion = \"9.9.9\"\n\
         lib = \"nested.wado\"\n",
    )
    .unwrap();
    fs::write(schemas.join("assets/nested.wado"), "").unwrap();

    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n\n\
         [dependencies]\n\"lib:schemas\" = { path = \"../schemas\" }\n\n\
         [build-dependencies]\n\"lib:gen\" = { path = \"../gen\" }\n",
    )
    .unwrap();
    let main = |primary: &str| {
        fs::write(
            app.join("src/main.wado"),
            format!(
                r#"use {{ println, Stdout }} from "core:cli";
use {{ a, b }} from "lib:schemas/{primary}" with {{
    generator: {{ module: "lib:gen", inputs: ["lib:schemas/assets/b.idl"] }},
}};

export fn run() with Stdout {{
    println(`${{a() + b()}}`);
}}
"#
            ),
        )
        .unwrap();
        wado_in(&app).args(["run", "src/main.wado"]).assert()
    };

    main("assets/a.idl").success().stdout("3\n");
    let [generated, record] = ["out.wado", "a.idl.kiln.json"].map(|file| {
        let found = fs::read_dir(app.join("build/kiln"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join(file))
            .find(|path| path.exists())
            .unwrap_or_else(|| panic!("no {file} under build/kiln"));
        fs::read_to_string(found).unwrap()
    });
    assert!(
        generated.starts_with(
            "#![generated(by = \"lib:gen\", sources = \
             [\"acme:schemas@0.2.0/assets/a.idl\", \"acme:schemas@0.2.0/assets/b.idl\"])]\n"
        ),
        "{generated}"
    );
    assert!(
        record.contains("\"acme:schemas@0.2.0/assets/a.idl\"")
            && !record.contains(root.to_str().unwrap()),
        "{record}"
    );
    assert!(
        !schemas.join("build").exists(),
        "the dependency's tree is not written"
    );

    // Moved elsewhere, the tree still hits the cache: nothing it records names
    // where a dependency lies.
    let generated_at = |app: &Path| {
        fs::read_dir(app.join("build/kiln"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("out.wado"))
            .find(|path| path.exists())
            .unwrap()
    };
    let written = fs::metadata(generated_at(&app))
        .unwrap()
        .modified()
        .unwrap();
    let moved = tmp.path().join("moved");
    fs::create_dir(&moved).unwrap();
    for package in ["gen", "schemas", "app"] {
        fs::rename(root.join(package), moved.join(package)).unwrap();
    }
    wado_in(&moved.join("app"))
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("3\n");
    let rewritten = generated_at(&moved.join("app"));
    assert_eq!(
        fs::metadata(rewritten).unwrap().modified().unwrap(),
        written,
        "a warm cache leaves the output as it was"
    );
    for package in ["gen", "schemas", "app"] {
        fs::rename(moved.join(package), root.join(package)).unwrap();
    }

    // Run from outside the package, the exported file is still found.
    fs::write(
        schemas.join("assets/b.idl"),
        "pub fn b() -> i32 { return 3; }\n",
    )
    .unwrap();
    wado_in(root)
        .args(["run", "app/src/main.wado"])
        .assert()
        .success()
        .stdout("4\n");

    main("assets/hidden.idl")
        .failure()
        .stderr(predicate::str::contains(
            "package `lib:schemas` exports no file `assets/hidden.idl`",
        ));
}

/// A clause in a file no `wado.toml` governs has no `[build-dependencies]` to
/// name its generator through, whatever its importer declares.
#[test]
fn a_clause_in_no_package_names_no_build_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    greeting_generator(&root.join("gen"), "hi");
    fs::create_dir_all(root.join("loose")).unwrap();
    fs::write(root.join("loose/schema.idl"), "anything\n").unwrap();
    fs::write(
        root.join("loose/part.wado"),
        r#"use { greeting } from "./schema.idl" with { generator: { module: "lib:gen" } };

pub fn part() -> String {
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
         [build-dependencies]\n\"lib:gen\" = { path = \"../gen\" }\n",
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { part } from "../../loose/part.wado";

export fn run() with Stdout {
    println(part());
}
"#,
    )
    .unwrap();
    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "kiln: generator `lib:gen` is not in the [build-dependencies] of \
             `../loose/part.wado`, which no wado.toml governs",
        ));
}

/// One schema two packages generate from: each clause resolves its generator
/// through its own package, so the two agree only where those generators are
/// one.
#[test]
fn two_packages_clauses_on_one_schema_merge_only_when_they_resolve_alike() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    greeting_generator(&root.join("gen_a"), "from a");
    greeting_generator(&root.join("gen_b"), "from b");

    let lib = root.join("lib");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n\n\
         [build-dependencies]\n\"lib:gen\" = { path = \"../gen_b\" }\n",
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
    let app_manifest = |generator: &str| {
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n\n\
             [build-dependencies]\n\"lib:gen\" = {{ path = \"../{generator}\" }}\n"
        )
    };
    fs::write(app.join("wado.toml"), app_manifest("gen_a")).unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { greeting } from "../../lib/src/schema.idl" with { generator: { module: "lib:gen" } };
use { hello } from "../../lib/src/lib.wado";

export fn run() with Stdout {
    println(`${greeting()} ${hello()}`);
}
"#,
    )
    .unwrap();

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("disagree"));

    fs::write(app.join("wado.toml"), app_manifest("gen_b")).unwrap();
    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from b from b"));
}
