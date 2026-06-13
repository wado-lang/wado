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
//! that exercises the full vertical slice: manifest parse → CompilerHost
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
    return `hello {subject()}`;
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
