//! A program `core:eval` evaluates imports the calling package's
//! `[dependencies]`, and its cached outcome follows the dependency's sources.
//! A fixture cannot lay out a second package, so this drives `wado test`.

use std::fs;
use std::path::Path;

use crate::common::wado_in;

/// `greet`, whose `hello` returns `greeting`, and `app`, whose test evaluates a
/// program printing `hello()` and asserts it printed `expected`.
fn write_packages(root: &Path, greeting: &str, expected: &str) {
    let greet = root.join("greet");
    fs::create_dir_all(greet.join("src")).unwrap();
    fs::write(
        greet.join("wado.toml"),
        "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        greet.join("src/lib.wado"),
        format!("use {{ part }} from \"./part.wado\";\npub fn hello() -> String {{ return part(\"{greeting}\"); }}\n"),
    )
    .unwrap();
    fs::write(
        greet.join("src/part.wado"),
        "pub fn part(s: String) -> String { return s; }\n",
    )
    .unwrap();

    let app = root.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ngreet = { path = \"../greet\" }\n",
    )
    .unwrap();
    fs::write(
        app.join("app_test.wado"),
        format!(
            r#"use {{ eval }} from "core:eval";

test "the program reaches the caller's dependency" {{
    let out = eval(`use {{ println, Stdout }} from "core:cli";
use {{ hello }} from "greet";
export fn run() with Stdout {{ println(hello()); }}`).unwrap();
    assert out.stdout == "{expected}\n", out.stdout;
}}
"#
        ),
    )
    .unwrap();
}

#[test]
fn an_evaluated_program_imports_the_callers_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    write_packages(tmp.path(), "hello", "hello");
    wado_in(&tmp.path().join("app"))
        .args(["test", "app_test.wado"])
        .assert()
        .success();
}

#[test]
fn editing_the_dependency_misses_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    write_packages(tmp.path(), "before", "before");
    wado_in(&app)
        .args(["test", "app_test.wado"])
        .assert()
        .success();

    // Only a file the dependency's entry imports changes, so the program's
    // source and the dependency's entry are what they were.
    write_packages(tmp.path(), "before", "before!");
    fs::write(
        tmp.path().join("greet/src/part.wado"),
        "pub fn part(s: String) -> String { return `${s}!`; }\n",
    )
    .unwrap();
    wado_in(&app)
        .args(["test", "app_test.wado"])
        .assert()
        .success();
}
