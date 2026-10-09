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

/// A test in `app` asserting the program printing `hello()` from `greet` fails
/// to compile, or prints `expected`.
fn write_expectation(app: &Path, expected: Option<&str>) {
    let check = match expected {
        None => "assert out matches { Err(_) };".to_string(),
        Some(text) => format!("assert out matches {{ Ok(o) && o.stdout == \"{text}\\n\" }};"),
    };
    fs::write(
        app.join("app_test.wado"),
        format!(
            r#"use {{ eval }} from "core:eval";

test "the program reaches the caller's dependency" {{
    let out = eval(`use {{ println, Stdout }} from "core:cli";
use {{ hello }} from "greet";
export fn run() with Stdout {{ println(hello()); }}`);
    {check}
}}
"#
        ),
    )
    .unwrap();
}

#[test]
fn a_dependency_file_that_appears_misses_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    write_packages(tmp.path(), "hello", "hello");
    let part = tmp.path().join("greet/src/part.wado");
    fs::remove_file(&part).unwrap();
    write_expectation(&app, None);
    wado_in(&app)
        .args(["test", "app_test.wado"])
        .assert()
        .success();

    // The program is what it was: only the file its compile found missing is new.
    fs::write(&part, "pub fn part(s: String) -> String { return s; }\n").unwrap();
    write_expectation(&app, Some("hello"));
    wado_in(&app)
        .args(["test", "app_test.wado"])
        .assert()
        .success();
}

/// A registry dependency is a prebuilt component in the warm cache, outside
/// every source dependency's root.
#[test]
fn an_evaluated_program_imports_a_registry_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let lib = tmp.path().join("lib");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"demo-lib\"\nnamespace = \"acme\"\nversion = \"0.1.2\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.wado"),
        "export fn greet() -> String { return \"hi from the registry\"; }\n",
    )
    .unwrap();
    wado_in(&lib).args(["build", "--lib"]).assert().success();
    let cached = cache.join("ghcr.io/acme/demo-lib/0.1.2");
    fs::create_dir_all(&cached).unwrap();
    fs::copy(lib.join("build/lib.wasm"), cached.join("component.wasm")).unwrap();

    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[registries]\ndefault = \"oci://ghcr.io\"\n\n[dependencies]\n\"acme:demo-lib\" = { version = \"^0.1\" }\n",
    )
    .unwrap();
    fs::write(
        app.join("app_test.wado"),
        r#"use { eval } from "core:eval";

test "the program reaches the registry dependency" {
    let out = eval(`use { println, Stdout } from "core:cli";
use { greet } from "acme:demo-lib";
export fn run() with Stdout { println(greet()); }`).unwrap();
    assert out.stdout == "hi from the registry\n", out.stdout;
}
"#,
    )
    .unwrap();
    wado_in(&app)
        .env("WADO_ROOT", &cache)
        .args(["test", "app_test.wado"])
        .assert()
        .success();
}

#[test]
fn a_caller_inside_its_dependency_reads_none_of_its_own_files() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("lib");
    let app = lib.join("app");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::create_dir_all(&app).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.wado"),
        "pub fn one() -> String { return \"1\"; }\n",
    )
    .unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib = { path = \"..\" }\n",
    )
    .unwrap();
    fs::write(app.join("own.wado"), "pub fn two() -> i32 { return 2; }\n").unwrap();
    fs::write(
        app.join("app_test.wado"),
        r#"use { eval } from "core:eval";

test "the dependency is read and the caller's own file is not" {
    let reached = eval(`use { println, Stdout } from "core:cli";
use { one } from "lib";
export fn run() with Stdout { println(one()); }`);
    assert reached matches { Ok(o) && o.stdout == "1\n" };
    let own = eval(`use { two } from "./own.wado";
export fn run() { let _ = two(); }`);
    assert own matches { Err(_) };
}
"#,
    )
    .unwrap();
    wado_in(&app)
        .args(["test", "app_test.wado"])
        .assert()
        .success();
}
