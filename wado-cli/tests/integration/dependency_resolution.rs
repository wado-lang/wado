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

use crate::common::wado_in;

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

    // A module the `[world]` table does not name checks as a library: no world
    // entry point is required of it (issue #2059).
    wado_in(&greet)
        .args(["check", "src/lib.wado"])
        .assert()
        .success();
}

/// A registry dependency that `wado fetch` warmed but no `wado.lock` pins is
/// resolved from the cache by every tier — the build one (`run`, `check`) and
/// the offline one (`query`). Before, only the build tier resolved it and
/// `check` reported a lockfile nothing else asked for (issue #2059).
#[test]
fn a_cached_registry_dependency_resolves_without_a_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache = root.join("cache");

    // The published component: built here, then placed where a fetch caches it.
    let lib = root.join("lib");
    fs::create_dir_all(lib.join("src")).unwrap();
    fs::write(
        lib.join("wado.toml"),
        "[package]\nname = \"demo-lib\"\nnamespace = \"acme\"\nversion = \"0.1.2\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(
        lib.join("src/lib.wado"),
        "export fn greet() -> String {\n    return \"hi from the registry\";\n}\n",
    )
    .unwrap();
    wado_in(&lib).args(["build", "--lib"]).assert().success();

    let cached = cache.join("ghcr.io/acme/demo-lib/0.1.2");
    fs::create_dir_all(&cached).unwrap();
    fs::copy(lib.join("build/lib.wasm"), cached.join("component.wasm")).unwrap();

    let app = root.join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        r#"[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"

[registries]
default = "oci://ghcr.io"

[dependencies]
"acme:demo-lib" = { version = "^0.1" }
"#,
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { greet } from "acme:demo-lib";

export fn run() with Stdout {
    println(greet());
}
"#,
    )
    .unwrap();
    assert!(!app.join("wado.lock").exists());

    for args in [
        vec!["run", "src/main.wado"],
        vec!["check", "src/main.wado"],
        vec!["query", "diagnostics", "src/main.wado"],
    ] {
        wado_in(&app)
            .env("WADO_ROOT", &cache)
            .args(&args)
            .assert()
            .success();
    }
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

/// A `lib:` alias no `[dependencies]` entry declares is reported as that, naming
/// the manifest it is missing from.
#[test]
fn an_undeclared_alias_names_the_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        app.join("src/main.wado"),
        "use { x } from \"lib:nothere\";\n\nexport fn run() {\n    x();\n}\n",
    )
    .unwrap();

    wado_in(&app)
        .args(["check", "src/main.wado"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains(
                "dependency 'lib:nothere' is not declared in [dependencies] of ",
            )
            .and(predicate::str::contains("app/wado.toml")),
        );
}

/// Write a package at `dir` whose `src/lib.wado` is `lib`, with the
/// `[dependencies]` lines in `deps`.
fn library(dir: &std::path::Path, deps: &str, lib: &str) {
    exporting_library(dir, &[], deps, lib);
}

/// [`library`], whose `[package].exports` lists `exports`.
fn exporting_library(dir: &std::path::Path, exports: &[&str], deps: &str, lib: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    let exports = exports
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        dir.join("wado.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n\
             exports = [{exports}]\n\n[dependencies]\n{deps}"
        ),
    )
    .unwrap();
    fs::write(dir.join("src/lib.wado"), lib).unwrap();
}

/// A dependency's exported submodule is imported by `"<package>/<file>"`,
/// offers its `pub` items, resolves its own imports as a module of that
/// package, and is one module with the file its `lib` entry reaches.
#[test]
fn an_exported_file_is_imported_by_package_and_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    let shapes = root.join("shapes");
    exporting_library(
        &shapes,
        &["src/point.wado"],
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "pub use { Point } from \"./point.wado\";\n",
    );
    fs::write(
        shapes.join("src/point.wado"),
        "use { leaf } from \"lib:leaf\";\n\
         pub struct Point { pub x: i32 }\n\
         pub fn origin() -> Point { return Point { x: leaf() - 1 }; }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        "\"lib:shapes\" = { path = \"../shapes\" }\n",
        "use { Point } from \"lib:shapes\";\n\
         use { origin } from \"lib:shapes/src/point.wado\";\n\
         pub fn api() -> i32 { let p: Point = origin(); return p.x; }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .success();
}

/// A file the allowlist does not name is refused at package granularity,
/// whether or not it is there, and a case-only mismatch names the entry.
#[test]
fn an_unlisted_file_is_refused_by_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let shapes = root.join("shapes");
    exporting_library(&shapes, &["src/Point.wado"], "", "pub fn f() {}\n");
    fs::write(shapes.join("src/Point.wado"), "pub fn g() {}\n").unwrap();
    fs::write(shapes.join("src/hidden.wado"), "pub fn h() {}\n").unwrap();
    let app = root.join("app");
    let check = |specifier: &str| {
        library(
            &app,
            "\"lib:shapes\" = { path = \"../shapes\" }\n",
            &format!("use {{ g }} from \"{specifier}\";\npub fn api() {{ g(); }}\n"),
        );
        wado_in(&app)
            .args(["check", "src/lib.wado"])
            .assert()
            .failure()
    };
    for file in ["src/hidden.wado", "src/missing.wado"] {
        check(&format!("lib:shapes/{file}")).stderr(predicate::str::contains(format!(
            "package `lib:shapes` exports no file `{file}`\n"
        )));
    }
    check("lib:shapes/src/point.wado").stderr(predicate::str::contains(
        "package `lib:shapes` exports no file `src/point.wado`; \
         it exports `src/Point.wado`, which differs in case",
    ));
    check("lib:other/src/Point.wado").stderr(predicate::str::contains(
        "dependency 'lib:other' is not declared in [dependencies] of ",
    ));
}

/// A namespace may carry `-`, as a manifest name does.
#[test]
fn an_undeclared_hyphenated_namespace_names_the_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    library(
        &app,
        "",
        "use { f } from \"wado-lang:absent\";\npub fn api() { f(); }\n",
    );
    wado_in(&app)
        .args(["check", "src/lib.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "dependency 'wado-lang:absent' is not declared in [dependencies] of ",
        ));
}

/// A single-file dependency is its own package, with no manifest, even inside
/// another package's tree: that package's `[dependencies]` are not its own.
#[test]
fn a_single_file_dependency_declares_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    let host = root.join("host");
    library(
        &host,
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "pub fn h() {}\n",
    );
    fs::write(
        host.join("shared.wado"),
        "use { leaf } from \"lib:leaf\";\npub fn shared() -> i32 { return leaf(); }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        "\"lib:shared\" = { path = \"../host/shared.wado\" }\n",
        "use { shared } from \"lib:shared\";\npub fn api() -> i32 { return shared(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "dependency 'lib:leaf' is not declared",
        ));
}

/// A dependency's file reached by a path is still its module, and resolves
/// against the dependency's `[dependencies]`.
#[test]
fn a_dependency_file_reached_by_path_resolves_against_its_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    let mid = root.join("mid");
    library(
        &mid,
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "pub fn m() {}\n",
    );
    fs::write(
        mid.join("src/part.wado"),
        "use { leaf } from \"lib:leaf\";\npub fn part() -> i32 { return leaf(); }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        "\"lib:mid\" = { path = \"../mid\" }\n",
        "use { part } from \"../../mid/src/part.wado\";\npub fn api() -> i32 { return part(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .success();
}

/// A file of a package no manifest declares, reached by a path, resolves
/// against that package's `[dependencies]`, never its importer's.
#[test]
fn an_undeclared_package_file_resolves_against_its_own_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    let mid = root.join("mid");
    let part = "use { leaf } from \"lib:leaf\";\npub fn part() -> i32 { return leaf(); }\n";
    let app_lib =
        "use { part } from \"../../mid/src/part.wado\";\npub fn api() -> i32 { return part(); }\n";
    library(
        &mid,
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "pub fn m() {}\n",
    );
    fs::write(mid.join("src/part.wado"), part).unwrap();
    library(&root.join("app"), "", app_lib);
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .success();

    library(&mid, "", "pub fn m() {}\n");
    fs::write(mid.join("src/part.wado"), part).unwrap();
    library(
        &root.join("app"),
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        app_lib,
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "dependency 'lib:leaf' is not declared in [dependencies] of {}",
            mid.join("wado.toml").canonicalize().unwrap().display()
        )));
}

/// A package an undeclared package shares with its importer is one package,
/// even where the entry is named through a symlink.
#[cfg(unix)]
#[test]
fn a_shared_dependency_is_one_package_through_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("real");
    library(
        &root.join("leaf"),
        "",
        "pub struct Token { pub v: i32 }\npub fn make() -> Token { return Token { v: 1 }; }\n",
    );
    let leaf = "\"lib:leaf\" = { path = \"../leaf\" }\n";
    let mid = root.join("mid");
    library(&mid, leaf, "pub fn m() {}\n");
    fs::write(
        mid.join("src/part.wado"),
        "use { Token } from \"lib:leaf\";\npub fn take(t: Token) -> i32 { return t.v; }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        leaf,
        "use { make } from \"lib:leaf\";\nuse { take } from \"../../mid/src/part.wado\";\n\
         pub fn api() -> i32 { return take(make()); }\n",
    );
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    wado_in(&root.join("app"))
        .args(["check"])
        .arg(link.join("app/src/lib.wado"))
        .assert()
        .success();
}

/// A package reached by a path before another package declares it stays one
/// package: its files keep one identity however they are reached after.
#[test]
fn a_package_reached_by_a_path_then_declared_is_one_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let b = root.join("b");
    library(
        &b,
        "",
        "use { Token } from \"./x.wado\";\npub fn make() -> Token { return Token { v: 1 }; }\n",
    );
    fs::write(b.join("src/x.wado"), "pub struct Token { pub v: i32 }\n").unwrap();
    let a = root.join("a");
    library(
        &a,
        "\"lib:b\" = { path = \"../b\" }\n",
        "pub fn unused() {}\n",
    );
    fs::write(
        a.join("src/y.wado"),
        "use { make } from \"lib:b\";\nuse { Token } from \"../../b/src/x.wado\";\n\
         pub fn mint() -> Token { return make(); }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        "",
        "use { Token } from \"../../b/src/x.wado\";\nuse { mint } from \"../../a/src/y.wado\";\n\
         pub fn api() -> i32 { let t: Token = mint(); return t.v; }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .success();
}

/// A file of such a package reached by its name is named by its package.
#[test]
fn an_exported_file_of_a_package_reached_by_a_path_is_named_by_its_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let b = root.join("b");
    exporting_library(
        &b,
        &["src/sub/x.wado"],
        "",
        "pub fn make() -> i32 { return 1; }\n",
    );
    fs::create_dir(b.join("src/sub")).unwrap();
    fs::write(
        b.join("src/sub/x.wado"),
        "pub fn x() -> i32 { return 2; }\n",
    )
    .unwrap();
    library(
        &root.join("a"),
        "\"lib:b\" = { path = \"../b\" }\n",
        "use { x } from \"lib:b/src/sub/x.wado\";\npub fn ax() -> i32 { return x(); }\n",
    );
    library(
        &root.join("app"),
        "",
        "use { make } from \"../../b/src/lib.wado\";\nuse { ax } from \"../../a/src/lib.wado\";\n\
         pub fn api() -> i32 { return make() + ax(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["dump", "--modules", "src/lib.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\n  b@0.1.0/src/sub/x.wado\n"));
}

/// A dependency is named by the path the OS reaches it by, so a source tree
/// linked in from elsewhere still reaches it, as one package however reached.
#[cfg(unix)]
#[test]
fn a_dependency_is_keyed_by_the_path_the_os_reaches() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub struct Token { pub v: i32 }\npub fn make() -> Token { return Token { v: 1 }; }\n",
    );
    let leaf = "\"lib:leaf\" = { path = \"../leaf\" }\n";
    let mid = root.join("mid");
    library(&mid, leaf, "pub fn m() {}\n");
    fs::write(
        mid.join("src/part.wado"),
        "use { Token } from \"lib:leaf\";\npub fn take(t: Token) -> i32 { return t.v; }\n",
    )
    .unwrap();
    let app = root.join("app");
    library(&app, leaf, "pub fn unused() {}\n");
    // `src` lives two levels deeper than it appears, so `..` from it climbs
    // differently on disk than in its spelling.
    let far = root.join("far/away/src");
    fs::create_dir_all(&far).unwrap();
    fs::write(
        far.join("lib.wado"),
        "use { make } from \"lib:leaf\";\nuse { take } from \"../../../mid/src/part.wado\";\n\
         pub fn api() -> i32 { return take(make()); }\n",
    )
    .unwrap();
    fs::remove_dir_all(app.join("src")).unwrap();
    std::os::unix::fs::symlink(&far, app.join("src")).unwrap();
    wado_in(&app)
        .args(["check", "src/lib.wado"])
        .assert()
        .success();
}

/// A file in no package, reached by a path, declares no dependencies, whatever
/// its importer declares.
#[test]
fn a_file_in_no_package_declares_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    fs::create_dir_all(root.join("loose")).unwrap();
    fs::write(
        root.join("loose/part.wado"),
        "use { leaf } from \"lib:leaf\";\npub fn part() -> i32 { return leaf(); }\n",
    )
    .unwrap();
    library(
        &root.join("app"),
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "use { part } from \"../../loose/part.wado\";\npub fn api() -> i32 { return part(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "dependency 'lib:leaf' is not declared: no wado.toml governs this module",
        ));
}

/// A dependency's bare import resolves against its own `[dependencies]`, at
/// any depth, and never against its consumer's.
#[test]
fn a_dependency_resolves_its_imports_against_its_own_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    library(
        &root.join("leaf"),
        "",
        "pub fn leaf() -> i32 { return 1; }\n",
    );
    library(
        &root.join("mid"),
        "\"lib:leaf\" = { path = \"../leaf\" }\n",
        "use { leaf } from \"lib:leaf\";\npub fn mid() -> i32 { return leaf() + 1; }\n",
    );
    library(
        &root.join("app"),
        "\"lib:mid\" = { path = \"../mid\" }\n",
        "use { mid } from \"lib:mid\";\npub fn api() -> i32 { return mid(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .success();

    library(
        &root.join("mid"),
        "",
        "use { leaf } from \"lib:leaf\";\npub fn mid() -> i32 { return leaf() + 1; }\n",
    );
    library(
        &root.join("app"),
        "\"lib:mid\" = { path = \"../mid\" }\n\"lib:leaf\" = { path = \"../leaf\" }\n",
        "use { mid } from \"lib:mid\";\npub fn api() -> i32 { return mid(); }\n",
    );
    wado_in(&root.join("app"))
        .args(["check", "src/lib.wado"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("dependency 'lib:leaf' is not declared in [dependencies] of ")
                .and(predicate::str::contains("mid/wado.toml")),
        );
}

/// With no manifest at all, the same import says there is none to declare it in.
#[test]
fn an_undeclared_alias_without_a_manifest_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("main.wado"),
        "use { x } from \"lib:nothere\";\n\nexport fn run() {\n    x();\n}\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["check", "main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "dependency 'lib:nothere' is not declared: no wado.toml governs this module, \
             and the `use` gives no inline source",
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
        r#"export struct Point {
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
