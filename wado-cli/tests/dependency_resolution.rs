//! End-to-end tests for `[dependencies]` resolution via bare-name `use`.
//!
//! Per WEP 2026-02-14 §"Module Resolution with Dependencies", a bare name
//! (no `:`, no `./`/`../`, no `http(s)://`) in a `use ... from "<name>"`
//! clause resolves against the `[dependencies]` table in `wado.toml`. The
//! dependency's entry is its `[package].lib` module; only `export` items are
//! visible to the consumer. For wado-to-wado source dependencies the CM world
//! is not observable — the consumer just imports the exported functions.
//!
//! This is a black-box test driving the real `wado` binary against a
//! self-contained two-package layout under a tempdir, which is the only place
//! that exercises the full vertical slice: manifest parse → `CompilerHost`
//! dependency mapping → loader `resolve_import` → compile/run.

use predicates::prelude::*;
use std::fs;

mod common;
use common::wado_in;

/// Lay out an `app` package that depends, via a relative `path` dependency,
/// on a sibling `greet` library package, and import `greet`'s exported
/// `hello` by its bare dependency name.
#[test]
fn path_dependency_resolves_via_bare_name_use() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Library package: entry is `[package].lib`; `export fn` is its API.
    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        r#"[package]
name = "greet"
version = "0.1.0"
lib = "src/lib.wado"
"#,
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        r#"export fn hello() -> String {
    return "hello from greet";
}
"#,
    )
    .unwrap();

    // Application package: depends on `greet` by path, imports it by name.
    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        r#"[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"

[dependencies]
greet = { path = "../greet" }
"#,
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "greet";

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
        .stdout(predicate::str::contains("hello from greet"));

    // `wado check` must resolve bare-name dependencies too (same host-provided
    // dependency index as compile/run), not just `wado run`.
    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .success();
}

/// The dependency's own modules resolve relative imports inside the
/// dependency package: `greet`'s lib `use`s a sibling helper.
#[test]
fn dependency_internal_relative_import_resolves() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        r#"[package]
name = "greet"
version = "0.1.0"
lib = "src/lib.wado"
"#,
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        r#"use { subject } from "./helper.wado";

export fn hello() -> String {
    return `hello ${subject()}`;
}
"#,
    )
    .unwrap();
    fs::write(
        greet.join("src/helper.wado"),
        r#"pub fn subject() -> String {
    return "world";
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

[dependencies]
greet = { path = "../greet" }
"#,
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "greet";

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
        .stdout(predicate::str::contains("hello world"));
}

/// A declared path dependency whose package has no `[package].lib` is
/// reported precisely (not as a generic "invalid module path").
#[test]
fn dependency_without_lib_reports_precise_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // greet declares a package but no `lib` entry.
    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        r#"[package]
name = "greet"
version = "0.1.0"
"#,
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        "export fn hello() -> String { return \"hi\"; }\n",
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

[dependencies]
greet = { path = "../greet" }
"#,
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { hello } from "greet";

export fn run() {
    let _ = hello();
}
"#,
    )
    .unwrap();

    wado_in(&app)
        .args(["compile", "-o", "out.wasm", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("greet").and(
            predicate::str::contains("[package].lib").or(predicate::str::contains("declares no")),
        ));
}

/// Two separate consumer projects may use the SAME dependency alias
/// (`greet`) for DIFFERENT packages. The dependency index is built per
/// project (per compile, on a fresh interner), so each resolves to its own
/// package with no cross-contamination.
#[test]
fn same_alias_different_package_resolves_per_project() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    for (pkg, msg) in [("greet_a", "from A"), ("greet_b", "from B")] {
        let dir = root.join(pkg);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("wado.toml"),
            format!("[package]\nname = \"{pkg}\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n"),
        )
        .unwrap();
        fs::write(
            dir.join("src/lib.wado"),
            format!("export fn greeting() -> String {{ return \"{msg}\"; }}\n"),
        )
        .unwrap();
    }

    // Both apps alias the dependency as `greet`, pointing at different packages.
    for (app, dep, expected) in [
        ("app_a", "greet_a", "from A"),
        ("app_b", "greet_b", "from B"),
    ] {
        let dir = root.join(app);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("wado.toml"),
            format!(
                r#"[package]
name = "{app}"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"

[dependencies]
greet = {{ path = "../{dep}" }}
"#
            ),
        )
        .unwrap();
        fs::write(
            dir.join("src/main.wado"),
            r#"use { println, Stdout } from "core:cli";
use { greeting } from "greet";

export fn run() with Stdout {
    println(greeting());
}
"#,
        )
        .unwrap();

        wado_in(&dir)
            .args(["run", "src/main.wado"])
            .assert()
            .success()
            .stdout(predicate::str::contains(expected));
    }
}

/// Two different aliases pointing at the SAME package resolve to one module
/// identity, so a type defined in that package unifies across both import
/// paths (no duplicate compilation / mismatched types).
#[test]
fn two_aliases_for_one_package_share_type_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let shared = root.join("shared");
    fs::create_dir_all(shared.join("src")).unwrap();
    fs::write(
        shared.join("wado.toml"),
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        shared.join("src/lib.wado"),
        r#"pub struct Point {
    pub x: i32,
}

export fn origin() -> Point {
    return Point { x: 42 };
}

export fn get_x(p: Point) -> i32 {
    return p.x;
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

[dependencies]
a = { path = "../shared" }
b = { path = "../shared" }
"#,
    )
    .unwrap();
    // `origin` comes via alias `a`, `get_x` via alias `b`; the `Point` value
    // flows from one to the other and must be the same type.
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { origin } from "a";
use { get_x } from "b";

export fn run() with Stdout {
    let p = origin();
    println(`${get_x(p)}`);
}
"#,
    )
    .unwrap();

    wado_in(&app)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}
