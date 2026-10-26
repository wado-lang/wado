//! End-to-end tests for wado CLI
//!
//! Tests the CLI interface including argument parsing, subcommands,
//! and integration with the compiler.

use std::time::Duration;

use predicates::prelude::*;
#[cfg(unix)]
use wado_host::sandbox;

use crate::common::{custom_sections, wado, wado_in, wado_test};

/// A project under a directory whose name contains a space must compile. The
/// entry is passed by ABSOLUTE path so the Kiln harvest uses an absolute
/// resolve base containing the space — the exact input that a space would make
/// a non-URI and panic on (regression for #1417).
#[test]
fn test_compile_project_under_path_with_space() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("My Project");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("eval.wado"),
        "pub fn answer() -> i32 { return 42; }\n",
    )
    .unwrap();
    let main = dir.join("main.wado");
    std::fs::write(
        &main,
        "use { println, Stdout } from \"core:cli\";\n\
         use { answer } from \"./eval.wado\";\n\
         export fn run() with Stdout { println(`answer: ${answer()}`); }\n",
    )
    .unwrap();
    let out = dir.join("out.wasm");

    wado()
        .arg("compile")
        .arg("-o")
        .arg(&out)
        .arg(&main)
        .assert()
        .success();
    assert!(out.exists(), "expected out.wasm to be written");
}

/// `wado doc` documents a program without errors. Given one with errors, it
/// reports them and fails, since a page of it would answer wrongly.
#[test]
fn test_doc_reports_an_error_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.wado");
    std::fs::write(&file, "pub fn answer() -> i32 { return \"no\"; }\n").unwrap();

    wado()
        .arg("doc")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

/// Write a minimal manifest-driven CLI package under `dir` and return the
/// output path to compile to.
fn write_metadata_project(dir: &std::path::Path) {
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\n\
         namespace = \"acme\"\n\
         name = \"app\"\n\
         version = \"0.3.0\"\n\
         description = \"A demo\"\n\
         repository = \"https://github.com/acme/app\"\n\
         license = \"MIT\"\n\
         authors = [\"Alice\"]\n\
         repository-directory = \"sub/app\"\n\n\
         [world]\n\
         \"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.wado"), "export fn run() {}\n").unwrap();
}

#[test]
fn test_build_embeds_package_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("build")
        .arg("-o")
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    let value = |name: &str| {
        sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
    };
    assert_eq!(value("description").as_deref(), Some("A demo"));
    assert_eq!(value("version").as_deref(), Some("0.3.0"));
    assert_eq!(
        value("source").as_deref(),
        Some("https://github.com/acme/app")
    );
    assert_eq!(value("licenses").as_deref(), Some("MIT"));
    assert_eq!(value("authors").as_deref(), Some("Alice"));
    assert_eq!(
        value("org.wado-lang.package.repository-directory").as_deref(),
        Some("sub/app")
    );
}

#[test]
fn test_build_default_output_to_build_dir() {
    // Without -o, `wado build` writes build/<world>.wasm at the manifest root,
    // not into the source tree.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);

    wado_in(dir).arg("build").assert().success();

    assert!(
        dir.join("build").join("wasi-cli-command.wasm").exists(),
        "expected build/wasi-cli-command.wasm"
    );
    assert!(
        !dir.join("src").join("main.wasm").exists(),
        "must not write into the source tree"
    );
}

#[test]
fn test_build_all_declared_worlds() {
    // A package with a library world and a hosted world: `wado build` (no
    // selector) builds both into build/, one artifact per world.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\n\
         namespace = \"acme\"\n\
         name = \"app\"\n\
         version = \"0.1.0\"\n\
         lib = \"src/lib.wado\"\n\n\
         [world]\n\
         \"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "export fn add(a: i32, b: i32) -> i32 { return a + b; }\n",
    )
    .unwrap();
    std::fs::write(dir.join("src").join("main.wado"), "export fn run() {}\n").unwrap();

    wado_in(dir).arg("build").assert().success();

    assert!(
        dir.join("build").join("lib.wasm").exists(),
        "expected build/lib.wasm for the library world"
    );
    assert!(
        dir.join("build").join("wasi-cli-command.wasm").exists(),
        "expected build/wasi-cli-command.wasm for the hosted world"
    );
}

#[test]
fn test_build_world_selector_builds_one() {
    // `--world <fq>` restricts the build to a single declared world.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);

    wado_in(dir)
        .args(["build", "--world", "wasi:cli/command"])
        .assert()
        .success();

    assert!(dir.join("build").join("wasi-cli-command.wasm").exists());
}

#[test]
fn test_compile_file_arg_does_not_embed_metadata() {
    // The same project, but the entry passed as an explicit file: a standalone
    // target, so no package metadata is embedded.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("compile")
        .arg("-o")
        .arg(&out)
        .arg("src/main.wado")
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        !sections.iter().any(|(n, _)| n == "description"),
        "file-arg compile must not embed metadata, got {sections:?}"
    );
}

#[test]
fn test_build_lib_embeds_component_type_without_interface_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"shapes\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "export struct Point { pub x: f64, pub y: f64 }\nexport fn identity(p: Point) -> Point { return p; }\n",
    )
    .unwrap();
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .arg("-o")
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        sections.iter().any(|(n, _)| n == "component-type"),
        "library build must embed the component-type section, got {sections:?}"
    );
}

#[test]
fn test_wit_lib_emits_root_world() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"shapes\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "export struct Point { pub x: f64, pub y: f64 }\nexport fn identity(p: Point) -> Point { return p; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("wit")
        .arg("--lib")
        .assert()
        .success()
        .stdout(predicate::str::contains("world root {"))
        .stdout(predicate::str::contains("interface shapes {"))
        .stdout(predicate::str::contains("world shapes").not());
}

/// A function `export(…)` maps onto a world export appears in the WIT as that
/// export, not under its own name.
#[test]
fn test_wit_names_mapped_export_by_world() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("app.wado");
    std::fs::write(
        &file,
        "use { Command } from \"wasi:cli\";\nexport(Command::run) fn main() {\n}\n",
    )
    .unwrap();

    wado()
        .arg("wit")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("export wasi:cli/run@"))
        .stdout(predicate::str::contains("export main").not());
}

#[test]
fn test_wit_lib_conflicts_with_world() {
    wado()
        .arg("wit")
        .arg("--lib")
        .arg("--world")
        .arg("wasi:cli/command")
        .assert()
        .failure()
        .stderr(predicate::str::contains("mutually exclusive"));
}

#[test]
fn test_build_os_skips_metadata() {
    // -Os strips symbols for minimal frontend delivery; package metadata is
    // dropped too, matching the WIT section.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("build")
        .arg("-Os")
        .arg("-o")
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        !sections.iter().any(|(n, _)| n == "description"),
        "-Os must not embed metadata, got {sections:?}"
    );
}

#[test]
fn test_build_os_embed_metadata_forces_on() {
    // --embed-metadata overrides the -Os default-off, mirroring --embed-wit.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("build")
        .arg("-Os")
        .arg("--embed-metadata")
        .arg("-o")
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        sections.iter().any(|(n, _)| n == "description"),
        "--embed-metadata must force metadata on under -Os, got {sections:?}"
    );
}

#[test]
fn test_build_no_embed_metadata_opts_out() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_metadata_project(dir);
    let out = dir.join("out.wasm");

    wado_in(dir)
        .arg("build")
        .arg("--no-embed-metadata")
        .arg("-o")
        .arg(&out)
        .assert()
        .success();

    let sections = custom_sections(&out);
    assert!(
        !sections.iter().any(|(n, _)| n == "description"),
        "--no-embed-metadata must opt out, got {sections:?}"
    );
}

#[test]
fn test_help() {
    wado()
        .arg("--help")
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado <command>"))
        .stderr(predicate::str::contains("compile"))
        .stderr(predicate::str::contains("run"));
}

/// The one-line summary in the command list must describe the whole
/// subcommand, matching `wado check --help`.
#[test]
fn test_help_summarizes_check_as_verifying_a_source_file() {
    wado()
        .arg("--help")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "check [options] [file.wado | dir]   Verify a source file and its Kiln generators",
        ));
}

/// `--help` must print usage rather than start the language server on stdio.
#[test]
fn test_lsp_help_does_not_start_the_server() {
    wado()
        .args(["lsp", "--help"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Usage: wado lsp"));
}

#[test]
fn test_run_propagates_a_guest_exit_code() {
    // `wasi:cli/exit` reaches wasmtime as `Err(I32Exit)`, indistinguishable
    // from a trap unless it is classified.
    wado()
        .args(["run", "wado-cli/tests/fixtures/run_exit_code.wado"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("run_exit_code: bad input"))
        .stderr(predicate::str::contains("Runtime error").not())
        .stderr(predicate::str::contains("wasm backtrace").not());
}

/// A profiled run refuses what it cannot honour before the guest runs, so
/// neither stands in for what the run itself would report.
#[test]
fn test_run_profile_refuses_a_bad_budget_or_path_before_running() {
    wado()
        .env("WADO_PROFILE_MAX_SECS", "30s")
        .args([
            "run",
            "--profile",
            "guest,scratchpad/profile.json",
            "wado-cli/tests/fixtures/run_exit_code.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not '30s'"))
        .stderr(predicate::str::contains("run_exit_code: bad input").not());
    wado()
        .args([
            "run",
            "--profile",
            "guest,no-such-directory/profile.json",
            "wado-cli/tests/fixtures/run_exit_code.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot write the profile to"))
        .stderr(predicate::str::contains("run_exit_code: bad input").not());
}

/// A profile that fails to write as the run ends does not replace what the run
/// reported: `/dev/full` opens, and every write to it fails.
#[cfg(target_os = "linux")]
#[test]
fn test_run_reports_its_own_outcome_over_a_profile_it_could_not_write() {
    wado()
        .args([
            "run",
            "--profile",
            "guest,/dev/full",
            "wado-cli/tests/fixtures/run_exit_code.wado",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("run_exit_code: bad input"))
        .stderr(predicate::str::contains("No space left on device"));
}

#[test]
fn test_version() {
    wado()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("wado "))
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_no_args_shows_usage() {
    wado()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: wado"));
}

#[test]
fn test_unknown_command() {
    wado()
        .arg("unknown")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown command"));
}

#[test]
fn test_compile_wat_to_stdout() {
    // Subprocess-only because `--wat-to-stdout` writes to actual stdout;
    // capturing it in-process would require redirecting std file
    // descriptors, which we have not done yet. The file-output and
    // opt-level variants moved to `tests/run_inprocess.rs`.
    wado()
        .args(["compile", "--wat-to-stdout", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(component"));
}

#[test]
fn test_compile_world_test_exports_tests() {
    // `--world test` targets the synthetic test world: the entry module's
    // `test` blocks become component exports (kebab-cased, plus a
    // `org.wado-lang.test-names` custom section), and no `run` entry point is required.
    wado()
        .args([
            "compile",
            "--world",
            "test",
            "--wat-to-stdout",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-0-simple"))
        .stdout(predicate::str::contains("org.wado-lang.test-names"));
}

#[test]
fn test_compile_world_default_requires_run() {
    // Without `--world test` the same file targets `wasi:cli/command`, which
    // requires a `run` entry point the fixture does not define — proof the
    // world selection actually changes compilation.
    wado()
        .args([
            "compile",
            "--wat-to-stdout",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));
}

#[test]
fn test_check_world_test_accepts_test_only_module() {
    // `wado check --world test` type-checks against the test world, so a
    // module with only `test` blocks (no `run`) passes.
    wado()
        .args([
            "check",
            "--world",
            "test",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success();
}

#[test]
fn test_check_world_default_checks_a_non_entry_module_as_a_library() {
    // A module no `[world]` entry names is a library, so `check` requires no
    // world entry point of it (issue #2059).
    wado()
        .args(["check", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success();
}

#[test]
fn test_check_named_world_requires_its_entry_point() {
    // Naming the world opts into its entry-point contract, which this fixture
    // does not satisfy.
    wado()
        .args([
            "check",
            "--world",
            "wasi:cli/command",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));
}

#[test]
fn test_check_with_no_file_checks_every_declared_world() {
    // `check` takes `build`'s targets: the library world and each `[world]`
    // entry, each against its own contract. The CLI world's entry is missing
    // its `run`, which is what the check has to catch.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\n\
         name = \"checkall\"\n\
         namespace = \"wado\"\n\
         version = \"0.1.0\"\n\
         lib = \"lib.wado\"\n\n\
         [world.\"wasi:cli/command\"]\n\
         entry = \"main.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.wado"),
        "export fn answer() -> i32 { return 42; }\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.wado"), "fn helper() -> i32 { return 1; }\n").unwrap();

    wado()
        .current_dir(dir)
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));

    // The library world alone is satisfied, so selecting it passes.
    std::fs::write(dir.join("main.wado"), "export fn run() { let _ = 1; }\n").unwrap();
    wado().current_dir(dir).arg("check").assert().success();
}

/// Checking a package decides unused items over all its files: a test file's
/// helper is test code, and an `internal` item another file calls is used.
#[test]
fn test_check_decides_unused_items_over_the_package() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\n\
         name = \"unused\"\n\
         namespace = \"wado\"\n\
         version = \"0.1.0\"\n\
         lib = \"lib.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.wado"),
        "use { helper } from \"./impl.wado\";\n\
         pub fn api() -> i32 { return helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("impl.wado"),
        "internal fn helper() -> i32 { return 1; }\n\
         internal fn tested_only() -> i32 { return 2; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("impl_test.wado"),
        "use { tested_only } from \"./impl.wado\";\n\
         fn check(n: i32) -> bool { return n > 0; }\n\
         test { assert check(tested_only()); }\n",
    )
    .unwrap();

    wado()
        .current_dir(dir)
        .arg("check")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "function `tested_only` is only used by tests",
        ))
        .stderr(predicate::str::contains("`helper`").not())
        .stderr(predicate::str::contains("`check`").not());
}

/// A package's entries offer its API however the manifest spells their path
/// and whether or not its tests cover them.
#[test]
fn test_check_offers_the_api_of_every_declared_entry() {
    let cases = [
        ("lib = \"src/../lib.wado\"\n", ""),
        ("lib = \"lib.wado\"\n", "[test]\nexclude = [\"lib.wado\"]\n"),
    ];
    for (lib, test) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("wado.toml"),
            format!(
                "[package]\nname = \"api\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\n{lib}\n{test}"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("lib.wado"),
            "use { helper } from \"./impl.wado\";\n\
             pub fn api() -> i32 { return helper(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("impl.wado"),
            "internal fn helper() -> i32 { return 1; }\n",
        )
        .unwrap();

        wado()
            .current_dir(dir)
            .arg("check")
            .assert()
            .success()
            .stderr(predicate::str::contains("never used").not());
    }
}

/// A lint reports in the package's own modules alone, checked as a package or
/// as one file: a dependency is not the consumer's to fix.
#[test]
fn test_check_reports_no_lint_in_a_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let greet = tmp.path().join("greet");
    std::fs::create_dir_all(&greet).unwrap();
    std::fs::write(
        greet.join("wado.toml"),
        "[package]\nname = \"greet\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"a.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        greet.join("a.wado"),
        "use { Doubled } from \"./t.wado\";\n\
         use { TreeSet } from \"core:collections\";\n\
         pub fn n() -> i32 { let n = 21; return n.doubled(); }\n\
         fn dead() {}\n",
    )
    .unwrap();
    std::fs::write(
        greet.join("t.wado"),
        "pub trait Doubled with () { fn doubled(&self) -> i32; }\n\
         impl Doubled for i32 { fn doubled(&self) -> i32 { return *self * 2; } }\n",
    )
    .unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"lib.wado\"\n\n\
         [dependencies]\n\"wado:greet\" = { path = \"../greet\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("lib.wado"),
        "use { n } from \"wado:greet\";\npub fn api() -> i32 { return n(); }\n",
    )
    .unwrap();

    for args in [&["check"][..], &["check", "lib.wado"]] {
        wado()
            .current_dir(&app)
            .args(args)
            .assert()
            .success()
            .stderr(predicate::str::contains("greet").not());
    }
}

/// A file outside the package's directory belongs to the package whose
/// manifest encloses it, or is a package of its own where none does. Its items
/// are not the consumer's to lint, and its `internal` items stay in its package.
#[test]
fn test_a_file_outside_the_package_is_another_package() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("wado.toml"),
        "[package]\nname = \"other\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        other.join("o.wado"),
        "use { base } from \"./p.wado\";\n\
         pub fn o() -> i32 { return base(); }\n\
         internal fn o_internal() -> i32 { return 2; }\n\
         pub fn o_unused() {}\n\
         fn o_dead() {}\n",
    )
    .unwrap();
    std::fs::write(
        other.join("p.wado"),
        "internal fn base() -> i32 { return 1; }\n",
    )
    .unwrap();
    let loose = tmp.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::write(
        loose.join("s.wado"),
        "pub fn s() -> i32 { return 3; }\nfn s_dead() {}\n",
    )
    .unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"lib.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("lib.wado"),
        "use { o } from \"../other/o.wado\";\n\
         use { s } from \"../loose/s.wado\";\n\
         pub fn api() -> i32 { return o() + s(); }\n",
    )
    .unwrap();

    for args in [&["check"][..], &["check", "lib.wado"]] {
        wado()
            .current_dir(&app)
            .args(args)
            .assert()
            .success()
            .stderr(predicate::str::contains("never used").not())
            .stderr(predicate::str::contains("unused").not());
    }

    std::fs::write(
        app.join("lib.wado"),
        "use { o_internal } from \"../other/o.wado\";\n\
         pub fn api() -> i32 { return o_internal(); }\n",
    )
    .unwrap();
    wado()
        .current_dir(&app)
        .args(["check", "lib.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "symbol 'o_internal' is `internal` to '../other/o.wado' and cannot be imported from another package",
        ));
}

/// A relative import of a path dependency's file reaches the module the
/// dependency names, so a type it declares is one type either way.
#[test]
fn test_a_relative_import_into_a_path_dependency_is_the_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("wado.toml"),
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\nlib = \"o.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        other.join("o.wado"),
        "pub struct P { pub x: i32 }\npub fn make() -> P { return P { x: 1 }; }\n",
    )
    .unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\n\"lib:other\" = { path = \"../other\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.wado"),
        "use { P } from \"lib:other\";\n\
         use { make } from \"../other/o.wado\";\n\
         export fn run() { let p: P = make(); assert p.x == 1; }\n",
    )
    .unwrap();

    wado()
        .current_dir(&app)
        .args(["check", "main.wado"])
        .assert()
        .success();
}

/// An entry under no manifest declares no package boundary, so every file it
/// reaches is in its package.
#[test]
fn test_modules_in_no_package_share_one_scope() {
    let tmp = tempfile::tempdir().unwrap();
    // Above the workspace, so a file under it that no package holds is in none.
    std::fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nname = \"outer\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let ws = tmp.path().join("ws");
    for dir in ["script", "lib", "app", "pkg"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    std::fs::write(
        ws.join("wado-workspace.toml"),
        "[workspace]\nmembers = []\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("lib/x.wado"),
        "internal fn x() -> i32 { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("script/s.wado"),
        "use { x } from \"../lib/x.wado\";\npub fn s() -> i32 { return x(); }\n",
    )
    .unwrap();
    for package in ["app", "pkg"] {
        std::fs::write(
            ws.join(package).join("wado.toml"),
            format!("[package]\nname = \"{package}\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
    }
    std::fs::write(
        ws.join("pkg/p.wado"),
        "internal fn p() -> i32 { return 2; }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("app/lib.wado"),
        "use { s } from \"../script/s.wado\";\npub fn api() -> i32 { return s(); }\n",
    )
    .unwrap();

    for (dir, file) in [("script", "s.wado"), ("app", "lib.wado")] {
        wado()
            .current_dir(ws.join(dir))
            .args(["check", file])
            .assert()
            .success();
    }

    std::fs::write(
        ws.join("script/s.wado"),
        "use { p } from \"../pkg/p.wado\";\npub fn s() -> i32 { return p(); }\n",
    )
    .unwrap();
    wado()
        .current_dir(ws.join("script"))
        .args(["check", "s.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "symbol 'p' is `internal` to '../pkg/p.wado' and cannot be imported from another package",
        ));
}

/// A module the package's code reaches is the package's own, whether or not
/// `wado test` runs it.
#[test]
fn test_check_reports_a_test_excluded_module_the_package_reaches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("src/gen")).unwrap();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nname = \"pkg\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n\n\
         [test]\nexclude = [\"src/gen\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.wado"),
        "use { helper } from \"./gen/x.wado\";\npub fn api() -> i32 { return helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/gen/x.wado"),
        "pub fn helper() -> i32 { return 1; }\nfn dead_one() {}\n",
    )
    .unwrap();

    wado()
        .current_dir(dir)
        .arg("check")
        .assert()
        .success()
        .stderr(predicate::str::contains("`dead_one` is never used"));
}

/// `--no-lint`, `--lint` and `--lint-generated` set one value, so the last one
/// given decides where the lints report.
#[test]
fn test_lint_flags_choose_where_lints_report() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("main.wado"),
        "use { g } from \"./g.wado\";\nfn dead_own() {}\nexport fn run() { g(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("g.wado"),
        "#![generated(by = \"test\")]\npub fn g() {}\nfn dead_generated() {}\n",
    )
    .unwrap();

    let cases: [(&[&str], bool, bool); 5] = [
        (&[], true, false),
        (&["--lint-generated"], true, true),
        (&["--no-lint"], false, false),
        (&["--no-lint", "--lint"], true, false),
        (&["--lint-generated", "--no-lint"], false, false),
    ];
    for (flags, own, generated) in cases {
        let report = |name: &str, expected: bool| {
            let found = predicate::str::contains(format!("`{name}` is never used"));
            if expected {
                found.boxed()
            } else {
                found.not().boxed()
            }
        };
        wado()
            .current_dir(dir)
            .args(["compile", "--wat-to-stdout"])
            .args(flags)
            .arg("main.wado")
            .assert()
            .success()
            .stderr(report("dead_own", own))
            .stderr(report("dead_generated", generated));
    }
}

/// `wado check --no-lint` skips the package's unused pass, which only lints
/// need.
#[test]
fn test_check_no_lint_reports_no_unused_item() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nname = \"pkg\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"lib.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.wado"),
        "pub fn api() -> i32 { return 1; }\nfn dead_one() {}\n",
    )
    .unwrap();

    wado()
        .current_dir(dir)
        .arg("check")
        .assert()
        .success()
        .stderr(predicate::str::contains("`dead_one` is never used"));
    wado()
        .current_dir(dir)
        .args(["check", "--no-lint"])
        .assert()
        .success()
        .stderr(predicate::str::contains("never used").not());
}

/// A member's file name reaches the package's load as written, whatever
/// characters a string literal escapes.
#[test]
fn test_check_loads_a_member_whose_name_a_string_literal_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nname = \"pkg\"\nnamespace = \"wado\"\nversion = \"0.1.0\"\nlib = \"lib.wado\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("lib.wado"), "pub fn api() -> i32 { return 1; }\n").unwrap();
    std::fs::write(
        dir.join("a\"b_test.wado"),
        "use { api } from \"./lib.wado\";\nfn dead_one() {}\ntest { assert api() == 1; }\n",
    )
    .unwrap();

    wado()
        .current_dir(dir)
        .arg("check")
        .assert()
        .success()
        .stderr(predicate::str::contains("`dead_one` is never used"));
}

/// A directory names the project to check, so a workspace member is reachable
/// without changing directory first.
#[test]
fn test_check_takes_a_directory_as_the_project_to_check() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let pkg = dir.join("member");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("wado.toml"),
        "[package]\n\
         name = \"member\"\n\
         namespace = \"wado\"\n\
         version = \"0.1.0\"\n\
         lib = \"lib.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("lib.wado"),
        "export fn answer() -> i32 { return 42; }\n",
    )
    .unwrap();

    wado()
        .current_dir(dir)
        .args(["check", "member"])
        .assert()
        .success();

    std::fs::write(
        pkg.join("lib.wado"),
        "export fn answer() -> i32 { return; }\n",
    )
    .unwrap();
    wado()
        .current_dir(dir)
        .args(["check", "member"])
        .assert()
        .failure();
}

#[test]
fn test_library_export_reaching_a_resource_is_refused() {
    // A handle from another interface has no export index in the library's own
    // instance type, and the CM emitter used to panic on the missing entry. A
    // declaration the signature names carries one just as the signature does,
    // so the guard follows fields, payloads and a newtype base.
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        "export fn take(d: Descriptor) -> i32 { return 1; }",
        "export affine struct Holder { pub d: Descriptor }\n\
         export fn take(h: Holder) -> i32 { return 1; }",
        "export affine variant Payload { Open(Descriptor), Closed }\n\
         export fn take(p: Payload) -> i32 { return 1; }",
        "export type Handle = Descriptor;\n\
         export fn take(h: Handle) -> i32 { return 1; }",
    ];
    for (i, case) in cases.iter().enumerate() {
        let src = dir.path().join(format!("lib{i}.wado"));
        std::fs::write(
            &src,
            format!("use {{ Descriptor }} from \"wasi:filesystem\";\n{case}\n"),
        )
        .unwrap();
        wado()
            .args(["check", src.to_str().unwrap()])
            .assert()
            .failure()
            .stderr(predicate::str::contains("cannot carry a resource handle"));
    }
}

#[test]
fn test_library_export_reaching_a_resource_through_an_alias_is_refused() {
    // The signature spells the alias, and only the reference site says which
    // declaration that is. Matching the spelling against declared names finds
    // nothing, and the emitter reaches the missing entry and panics.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("other.wado"),
        "use { Descriptor } from \"wasi:filesystem\";\n\
         export affine struct Holder { pub d: Descriptor }\n",
    )
    .unwrap();
    let src = dir.path().join("lib.wado");
    std::fs::write(
        &src,
        "use { Holder as Held } from \"./other.wado\";\n\
         export fn take(h: Held) -> i32 { let _ = h; return 1; }\n",
    )
    .unwrap();

    wado()
        .args(["check", src.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot carry a resource handle"));
}

#[test]
fn test_run_hello() {
    wado()
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, world!"));
}

/// The fuel `wado run --report-fuel` reports on stderr.
fn reported_fuel(args: &[&str]) -> u64 {
    let out = wado().args(args).output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    stderr
        .lines()
        .find_map(|l| l.strip_prefix("fuel "))
        .unwrap_or_else(|| panic!("no fuel line on stderr:\n{stderr}"))
        .parse()
        .unwrap()
}

/// Fuel counts Wasm operators, so pure computation spends the same fuel on
/// every run, whatever the machine and its load.
#[test]
fn test_run_report_fuel_is_deterministic() {
    let args = [
        "run",
        "--report-fuel",
        "wado-cli/tests/fixtures/run_pure.wado",
    ];
    let first = reported_fuel(&args);
    assert!(first > 0);
    assert_eq!(first, reported_fuel(&args));
}

/// A guest that exits on purpose still spent fuel getting there.
#[test]
fn test_run_reports_fuel_on_guest_exit() {
    let args = [
        "run",
        "--report-fuel",
        "wado-cli/tests/fixtures/run_exit_code.wado",
    ];
    assert!(reported_fuel(&args) > 0);
}

#[test]
fn test_run_reports_no_fuel_unasked() {
    wado()
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stderr(predicate::str::contains("fuel").not());
}

/// The program name is what `wado run` was given, never `wado` itself, and
/// `args()` holds what follows it.
#[test]
fn test_run_program_name_and_args() {
    wado()
        .args([
            "run",
            "wado-cli/tests/fixtures/run_args.wado",
            "--",
            "-x",
            "y",
        ])
        .assert()
        .success()
        .stdout(
            r#"Option::Some("wado-cli/tests/fixtures/run_args.wado") ["-x", "y"]"#.to_owned()
                + "\n",
        );
}

/// `example/cat.wado` prints every file it is given, in order, and not itself.
#[test]
fn test_run_cat_prints_each_file() {
    let hello = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../example/hello.wado"
    ))
    .unwrap();
    wado()
        .args([
            "run",
            "example/cat.wado",
            "example/hello.wado",
            "example/hello.wado",
        ])
        .assert()
        .success()
        .stdout(hello.repeat(2));
}

/// Like cat(1), `example/cat.wado` reports a file it cannot open, prints the
/// rest, and exits with a failure.
#[test]
fn test_run_cat_fails_on_a_missing_file() {
    let hello = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../example/hello.wado"
    ))
    .unwrap();
    wado()
        .args([
            "run",
            "example/cat.wado",
            "example/no-such-file",
            "example/hello.wado",
        ])
        .assert()
        .failure()
        .stdout(hello)
        .stderr(predicates::str::contains(
            "cat: example/no-such-file: cannot open: ErrorCode::NoEntry",
        ));
}

/// `core:args::from_env` parses what follows the program name.
#[test]
fn test_run_args_reach_from_env() {
    wado()
        .args([
            "run",
            "wado-cli/tests/fixtures/run_args_from_env.wado",
            "--jobs",
            "4",
            "in.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("input=in.txt jobs=4"));
}

/// A run writes the program's output and nothing else.
#[test]
fn test_run_announces_nothing() {
    wado()
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

/// Traces reach stderr, which every `optimizer-debug` recipe depends on.
#[test]
fn test_trace_env_var_reaches_stderr() {
    wado()
        .env("WADO_LIST_PASSES", "1")
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stderr(predicate::str::contains("[pass] nir/"));
}

#[test]
fn test_test_passing() {
    wado_test()
        .args(["wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_installs_the_stdlib_before_anything_reaches_it() {
    // A dev build takes the stdlib from its host, and the prewarm workers
    // reached it first: one swallowed panic per worker (issue #2102).
    wado_test()
        .args(["wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stderr(predicate::str::contains("panicked").not());
}

#[test]
fn test_test_evaluates_on_a_single_cpu_permit() {
    // An `eval` compile takes its test's only permit, so the test must be able
    // to take it back once the compile ends.
    wado_test()
        .args(["--parallel", "1", "wado-compiler/lib/core/eval_test.wado"])
        .timeout(Duration::from_secs(300))
        .assert()
        .success()
        .stdout(predicate::str::contains(" 0 failed"));
}

#[test]
fn test_test_failing() {
    wado_test()
        .args(["wado-cli/tests/fixtures/test_fail.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("1 passed, 1 failed"));
}

#[test]
fn test_test_skips_a_passed_file_until_something_it_read_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep.wado");
    let body = |n: i32| format!("pub fn two() -> i32 {{\n    return {n};\n}}\n");
    std::fs::write(&dep, body(2)).unwrap();
    let main = tmp.path().join("main_test.wado");
    std::fs::write(
        &main,
        "use { two } from \"./dep.wado\";\n\ntest \"adds\" {\n    assert two() + 1 == 3;\n}\n",
    )
    .unwrap();
    let run = || wado().arg("test").arg(&main).assert();

    run()
        .success()
        .stdout(predicate::str::contains("cached:").not());
    run()
        .success()
        .stdout(predicate::str::contains("cached:  1 files"))
        .stdout(predicate::str::contains("1 passed, 0 failed"));
    wado_test()
        .arg(&main)
        .assert()
        .success()
        .stdout(predicate::str::contains("cached:").not());

    std::fs::write(&dep, body(3)).unwrap();
    run()
        .failure()
        .stdout(predicate::str::contains("0 passed, 1 failed"));
    // A failure is never cached.
    run()
        .failure()
        .stdout(predicate::str::contains("cached:").not());
    // A file in no package caches in the user cache, never beside itself.
    assert!(!tmp.path().join("build").exists());
}

/// An environment variable the compile read is an input like a file: a run
/// that sees another value compiles again.
#[test]
fn test_test_caches_a_file_whose_compile_read_the_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("param_test.wado");
    std::fs::write(
        &main,
        "#[param(from_env = \"PARAM_TEST_N\")]\nglobal N: i32 = 1;\n\ntest \"t\" {\n    assert N == 1;\n}\n",
    )
    .unwrap();
    let run = || wado().arg("test").arg(&main).assert();

    run().success();
    run()
        .success()
        .stdout(predicate::str::contains("cached:  1 files"));
    wado()
        .env("PARAM_TEST_N", "2")
        .arg("test")
        .arg(&main)
        .assert()
        .failure()
        .stdout(predicate::str::contains("0 passed, 1 failed"));
}

/// `wado run` takes the component it built from the cache while nothing the
/// compile read has changed, and a file in no package keeps that cache in the
/// user cache rather than beside itself.
#[cfg(unix)]
#[test]
fn test_run_reuses_its_component_until_something_it_read_changes() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let dep = scripts.join("dep.wado");
    let body = |n: i32| format!("pub fn n() -> i32 {{\n    return {n};\n}}\n");
    std::fs::write(&dep, body(1)).unwrap();
    let main = scripts.join("main.wado");
    std::fs::write(
        &main,
        "use { println, Stdout } from \"core:cli\";\nuse { n } from \"./dep.wado\";\n\nexport fn run() with Stdout {\n    println(`${n()}`);\n}\n",
    )
    .unwrap();
    let run = |expected: &str| {
        wado_in(&scripts)
            .env("XDG_CACHE_HOME", cache.path())
            .args(["run", "main.wado"])
            .assert()
            .success()
            .stdout(format!("{expected}\n"));
    };
    let entry = || {
        let dir = cache.path().join("wado/run");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        assert_eq!(entries.len(), 1, "one entry per entry file and options");
        std::fs::metadata(&entries[0]).unwrap().ino()
    };

    run("1");
    let first = entry();
    run("1");
    assert_eq!(entry(), first, "an unchanged run writes no entry");
    std::fs::write(&dep, body(2)).unwrap();
    run("2");
    assert_ne!(entry(), first, "an edited import compiles again");
    assert_eq!(
        std::fs::read_dir(&scripts).unwrap().count(),
        2,
        "nothing is written beside the script"
    );
}

/// A cache entry names every file the compile read, and a path that is not
/// UTF-8 cannot be written into one: the run goes ahead uncached.
#[cfg(unix)]
#[test]
fn test_run_compiles_a_file_under_a_directory_whose_name_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join(std::ffi::OsStr::from_bytes(b"not-utf8-\xff"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.wado"),
        "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hi\");\n}\n",
    )
    .unwrap();
    wado_in(&dir)
        .args(["run", "main.wado"])
        .assert()
        .success()
        .stdout("hi\n");
}

/// The package metadata a project's component embeds is read outside the
/// compile, so the cached component is checked against it too.
#[test]
fn test_run_rebuilds_a_cached_component_once_its_license_file_changes() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("wado.toml"),
        "[package]\nname = \"lic\"\nversion = \"0.1.0\"\nlicense-file = \"LICENSE\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/main.wado"),
        "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hi\");\n}\n",
    )
    .unwrap();
    let embedded_license = |text: &str| {
        std::fs::write(tmp.path().join("LICENSE"), text).unwrap();
        wado_in(tmp.path())
            .args(["run", "src/main.wado"])
            .assert()
            .success();
        let cached: Vec<_> = std::fs::read_dir(tmp.path().join("build/run"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "wasm"))
            .collect();
        assert_eq!(cached.len(), 1);
        custom_sections(&cached[0])
            .into_iter()
            .find(|(name, _)| name == "org.wado-lang.license")
            .map(|(_, payload)| String::from_utf8(payload).unwrap())
    };
    assert_eq!(embedded_license("one").as_deref(), Some("one"));
    assert_eq!(embedded_license("two").as_deref(), Some("two"));
}

#[test]
fn test_clean_global_removes_the_user_cache() {
    let cache = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(cache.path().join("wado/run")).unwrap();
    std::fs::write(cache.path().join("wado/run/x.json"), "{}").unwrap();
    wado()
        .env("XDG_CACHE_HOME", cache.path())
        .args(["clean", "--global"])
        .assert()
        .success();
    assert!(!cache.path().join("wado").exists());
    assert!(cache.path().exists(), "only wado's own directory goes");
    wado()
        .args(["clean", "--global", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass one"));
}

#[test]
fn test_clean_global_ignores_a_relative_xdg_cache_home() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".cache/wado")).unwrap();
    std::fs::create_dir_all(cwd.path().join("rel/wado")).unwrap();
    wado_in(cwd.path())
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", "rel")
        .args(["clean", "--global"])
        .assert()
        .success();
    assert!(cwd.path().join("rel/wado").exists());
    assert!(!home.path().join(".cache/wado").exists());
}

#[test]
fn test_test_reruns_a_passed_file_once_a_transitive_path_dependency_names_another_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let greet = tmp.path().join("greet");
    std::fs::create_dir_all(&greet).unwrap();
    let manifest =
        |lib: &str| format!("[package]\nname = \"greet\"\nversion = \"0.1.0\"\nlib = \"{lib}\"\n");
    std::fs::write(greet.join("wado.toml"), manifest("a.wado")).unwrap();
    std::fs::write(greet.join("a.wado"), "pub fn n() -> i32 { return 1; }\n").unwrap();
    std::fs::write(greet.join("b.wado"), "pub fn n() -> i32 { return 2; }\n").unwrap();
    let mid = tmp.path().join("mid");
    std::fs::create_dir_all(&mid).unwrap();
    std::fs::write(
        mid.join("wado.toml"),
        "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nlib = \"mid.wado\"\n\n[dependencies]\ngreet = { path = \"../greet\" }\n",
    )
    .unwrap();
    std::fs::write(mid.join("mid.wado"), "pub use { n } from \"greet\";\n").unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nmid = { path = \"../mid\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("app_test.wado"),
        "use { n } from \"mid\";\n\ntest \"one\" {\n    assert n() == 1;\n}\n",
    )
    .unwrap();
    let run = || wado().arg("test").arg(app.join("app_test.wado")).assert();

    run().success();
    run()
        .success()
        .stdout(predicate::str::contains("cached:  1 files"));
    std::fs::write(greet.join("wado.toml"), manifest("b.wado")).unwrap();
    run()
        .failure()
        .stdout(predicate::str::contains("0 passed, 1 failed"));
}

/// A file's package is decided by the manifests above it, so adding one reruns
/// a passed file that a relative import reaches across it.
#[test]
fn test_test_reruns_a_passed_file_once_a_package_boundary_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("wado.toml"),
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        other.join("o.wado"),
        "use { base } from \"./inner/p.wado\";\npub fn o() -> i32 { return base(); }\n",
    )
    .unwrap();
    std::fs::create_dir_all(other.join("inner")).unwrap();
    std::fs::write(
        other.join("inner/p.wado"),
        "internal fn base() -> i32 { return 1; }\n",
    )
    .unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("a_test.wado"),
        "use { o } from \"../other/o.wado\";\n\ntest \"one\" {\n    assert o() == 1;\n}\n",
    )
    .unwrap();
    let run = || wado().arg("test").arg(app.join("a_test.wado")).assert();

    run().success();
    run()
        .success()
        .stdout(predicate::str::contains("cached:  1 files"));
    std::fs::write(
        other.join("inner/wado.toml"),
        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    run()
        .failure()
        .stdout(predicate::str::contains("cached:").not());
}

/// The package a module is in is the one its file is in on disk, `..` taken
/// after a symlinked directory as the OS takes it.
#[cfg(unix)]
#[test]
fn test_a_package_root_is_found_where_the_module_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let pkgs = tmp.path().join("real/pkgs");
    std::fs::create_dir_all(pkgs.join("app")).unwrap();
    std::fs::create_dir_all(pkgs.join("other")).unwrap();
    std::fs::write(
        pkgs.join("other/wado.toml"),
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        pkgs.join("other/o.wado"),
        "use { base } from \"./p.wado\";\npub fn o() -> i32 { return base(); }\n",
    )
    .unwrap();
    std::fs::write(
        pkgs.join("other/p.wado"),
        "internal fn base() -> i32 { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        pkgs.join("app/wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        pkgs.join("app/main.wado"),
        "use { o } from \"../other/o.wado\";\nexport fn run() { assert o() == 1; }\n",
    )
    .unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(pkgs.join("app"), work.join("app")).unwrap();

    wado()
        .current_dir(&work)
        .args(["check", "app/main.wado"])
        .assert()
        .success();
}

#[test]
fn test_test_keys_its_cache_by_the_compilers_environment() {
    // The compiler reads `WADO_*` itself, and they change what it emits.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("env_test.wado");
    std::fs::write(&main, "test \"t\" {\n    assert 1 + 1 == 2;\n}\n").unwrap();
    wado().arg("test").arg(&main).assert().success();
    wado()
        .env("WADO_SKIP_PASS", "inline")
        .arg("test")
        .arg(&main)
        .assert()
        .success()
        .stdout(predicate::str::contains("cached:").not());
}

#[test]
fn test_test_reports_on_every_run_what_a_cache_cannot_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty_test.wado");
    std::fs::write(&empty, "pub fn f() -> i32 {\n    return 1;\n}\n").unwrap();
    let fuel = tmp.path().join("fuel_test.wado");
    std::fs::write(&fuel, "test \"t\" {\n    assert 1 + 1 == 2;\n}\n").unwrap();
    for _ in 0..2 {
        // A file without tests is a skip, which says it holds none.
        wado()
            .arg("test")
            .arg(&empty)
            .assert()
            .success()
            .stdout(predicate::str::contains("skip:"))
            .stdout(predicate::str::contains("cached:").not());
        wado()
            .args(["test", "--report-fuel"])
            .arg(&fuel)
            .assert()
            .success()
            .stdout(predicate::str::is_match(r"fuel \d+").unwrap());
    }
}

/// A test blocked on a host future runs no wasm, so no epoch interrupts it; the
/// timeout still ends it.
#[cfg(unix)]
#[test]
fn test_test_times_out_a_guest_blocked_on_the_host() {
    if sandbox::availability() != sandbox::Availability::Available {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("blocked_test.wado");
    std::fs::write(
        &file,
        "use { cmd } from \"core:process\";\n\n#[timeout_ms(300)]\ntest \"blocked\" {\n    let _ = cmd`sleep 30`.status();\n}\n",
    )
    .unwrap();
    wado_test()
        .arg(&file)
        .timeout(Duration::from_secs(60))
        .assert()
        .failure()
        .stdout(predicate::str::contains("test timed out after 300ms"));
}

/// `--process-read` lets a child read a path outside the guest's grants, and
/// that path alone.
#[cfg(unix)]
#[test]
fn test_run_process_read_opens_a_path_to_the_child() {
    use std::os::unix::fs::PermissionsExt;

    if sandbox::availability() != sandbox::Availability::Available {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let tool = tempfile::tempdir().unwrap();
    std::fs::write(tool.path().join("data.txt"), "from the tool").unwrap();
    std::fs::write(
        work.path().join("main.wado"),
        "use { args, print, Stdout, Environment } from \"core:cli\";\nuse { cmd, Process } from \"core:process\";\n\nexport fn run() with (Stdout, Environment, Process) {\n    let path = args()[0];\n    print(String::from_utf8_lossy(&cmd`cat ${path}`.read().unwrap()));\n}\n",
    )
    .unwrap();
    let data = tool.path().join("data.txt");
    let run = |extra: &[&std::ffi::OsStr]| {
        wado_in(work.path())
            .arg("run")
            .args(extra)
            .arg("main.wado")
            .arg(&data)
            .assert()
    };

    run(&[]).failure();
    run(&["--process-read".as_ref(), tool.path().as_os_str()])
        .success()
        .stdout("from the tool");

    // `[process] read` in the user config opens it the same way, unless
    // another user can write the file.
    let config_home = tempfile::tempdir().unwrap();
    let config = config_home.path().join("wado").join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "[process]\nread = [{:?}]\n",
            tool.path().display().to_string()
        ),
    )
    .unwrap();
    let run_configured = || {
        wado_in(work.path())
            .env("XDG_CONFIG_HOME", config_home.path())
            .args(["run", "main.wado"])
            .arg(&data)
            .assert()
    };
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    run_configured().success().stdout("from the tool");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o666)).unwrap();
    run_configured()
        .failure()
        .stderr(predicate::str::contains(format!(
            "warning: ignoring {}: other users can write it",
            config.display()
        )));
}

/// A program the sandbox would refuse to execute is named, with the grant
/// that lets it run, once however often the guest tries it.
#[cfg(unix)]
#[test]
fn test_run_names_a_program_the_sandbox_refuses_and_its_grant() {
    use std::os::unix::fs::PermissionsExt;

    if sandbox::availability() != sandbox::Availability::Available {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let tool = tempfile::tempdir().unwrap();
    let tool_dir = tool.path().canonicalize().unwrap();
    let program = tool_dir.join("tool.sh");
    std::fs::write(&program, "#!/bin/sh\necho tool\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        work.path().join("main.wado"),
        "use { args, Environment } from \"core:cli\";\nuse { Command, Process } from \"core:process\";\n\nexport fn run() with (Environment, Process) {\n    for let _ of 0..<2 {\n        let _ = Command::new(args()[0]).status();\n    }\n}\n",
    )
    .unwrap();
    let warning = format!(
        "warning: core:process: `{}` is {}, outside the guest's grants and the system baseline\nhelp: let a child run it with `--process-read {}`",
        program.display(),
        program.display(),
        tool_dir.display()
    );
    let refused = wado_in(work.path())
        .args(["run", "main.wado"])
        .arg(&program)
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let refused = String::from_utf8(refused).unwrap();
    assert_eq!(refused.matches(&warning).count(), 1, "{refused}");
    wado_in(work.path())
        .args(["run", "--process-read"])
        .arg(&tool_dir)
        .arg("main.wado")
        .arg(&program)
        .assert()
        .success()
        .stderr(predicate::str::contains("core:process").not());
}

#[test]
fn test_run_unavailable_spawning_names_its_fix() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("main.wado"),
        "use { cmd, Process } from \"core:process\";\n\nexport fn run() with Process {\n    let _ = cmd`true`.status();\n}\n",
    )
    .unwrap();
    wado_in(tmp.path())
        .args(["run", "--deny-process", "main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "core:process is unavailable: spawning is turned off by --deny-process\nhelp: run without `--deny-process`\nhelp: a guest that can do without spawning asks `availability()` first",
        ));
}

#[test]
fn test_run_process_read_takes_an_existing_path_and_a_spawning_guest() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    wado()
        .args(["run", "--process-read"])
        .arg(&missing)
        .arg("example/hello.wado")
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "--process-read {}: ",
            missing.display()
        )));
    for flags in [
        ["--deny-process", "--process-read"],
        ["--process-read", "--deny-process"],
    ] {
        let mut cmd = wado();
        cmd.arg("run");
        for flag in flags {
            cmd.arg(flag);
            if flag == "--process-read" {
                cmd.arg(tmp.path());
            }
        }
        cmd.arg("example/hello.wado")
            .assert()
            .failure()
            .stderr(predicate::str::contains("which --deny-process forbids"));
    }
    wado()
        .args([
            "run",
            "--deny-process",
            "--deny-process",
            "example/hello.wado",
        ])
        .assert()
        .success();
}

#[test]
fn test_test_runs_a_file_named_twice_once() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("twice_test.wado");
    std::fs::write(
        &main,
        "test \"a\" {\n    assert 1 == 1;\n}\n\ntest \"b\" {\n    assert 2 == 2;\n}\n",
    )
    .unwrap();
    wado()
        .arg("test")
        .arg(tmp.path())
        .arg(&main)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_never_caches_a_file_taking_input_at_run_time() {
    // What it receives at run time is no input its compile recorded.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("random_test.wado");
    std::fs::write(
        &main,
        "use secure_random from \"core:secure_random\";\n\ntest \"draws\" {\n    assert secure_random::bytes(4).len() == 4;\n}\n",
    )
    .unwrap();
    for _ in 0..2 {
        wado()
            .args(["test"])
            .arg(&main)
            .assert()
            .success()
            .stdout(predicate::str::contains("cached:").not());
    }
}

#[test]
fn test_test_unexpectedly_passed_todo_says_what_the_mark_meant() {
    wado_test()
        .args(["wado-cli/tests/fixtures/test_todo_unexpected_pass.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "marked #[TODO] (expected to fail until the feature works), but it passed: \
             remove #[TODO] so it runs as a regular test",
        ))
        .stdout(predicate::str::contains("unexpectedly passed"))
        .stdout(predicate::str::contains("resolved").not());
}

#[test]
fn test_test_unexpectedly_passed_todo_in_tap() {
    wado_test()
        .args([
            "--format",
            "tap",
            "wado-cli/tests/fixtures/test_todo_unexpected_pass.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("# TODO unexpectedly passed"))
        .stdout(predicate::str::contains("resolved").not());
}

#[test]
fn test_test_unexpectedly_passed_todo_in_a_todo_module_points_at_the_module() {
    wado_test()
        .args(["wado-cli/tests/fixtures/test_todo_module_unexpected_pass.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "its module is #![TODO] (every test expected to fail until the feature works), \
             but this test passed: replace #![TODO] with #[TODO] on each test that still fails",
        ))
        .stdout(predicate::str::contains("remove #[TODO]").not());
}

#[test]
fn test_test_trap_names_its_reason() {
    // The reason is the cause under wasmtime's backtrace context: a plain
    // `Display` leaves it out, and `Debug` adds the host's own backtrace.
    wado_test()
        .args(["wado-cli/tests/fixtures/test_trap.wado"])
        .env("RUST_BACKTRACE", "1")
        .assert()
        .failure()
        .stdout(predicate::str::contains("integer divide by zero"))
        .stdout(predicate::str::contains("Stack backtrace").not());
}

#[test]
fn test_test_heartbeat_is_default_and_reports_failure_immediately() {
    // `heartbeat` is the default `--format`: no per-file `Compiled`/`Loaded`
    // log lines and no per-test `ok`/`FAILED` lines (that's `verbose`
    // territory) — but a failing test still gets its own `not ok` line,
    // not just a buried count in the final summary.
    wado_test()
        .args(["wado-cli/tests/fixtures/test_fail.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("not ok"))
        .stdout(predicate::str::contains("failing"))
        .stdout(predicate::str::contains("1 passed, 1 failed"))
        .stdout(predicate::str::contains("Compiled").not())
        .stdout(predicate::str::contains("Running tests in").not());
}

#[test]
fn test_test_heartbeat_discards_output_from_passing_tests() {
    // A passing test's own stdout is captured (not left to race directly
    // onto the real stdout — see `runtime::create_test_store`); heartbeat
    // discards it since there's nothing that needs attention.
    wado_test()
        .args(["wado-cli/tests/fixtures/test_stdout.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from the test body").not());
}

#[test]
fn test_test_heartbeat_shows_captured_stderr_on_failure() {
    // A failing test's captured stderr is attached to its immediate
    // failure notice — not just discarded like a passing test's output.
    wado_test()
        .args(["wado-cli/tests/fixtures/test_fail.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("stderr:"))
        .stdout(predicate::str::contains("this should fail"));
}

#[test]
fn test_test_verbose_shows_captured_stdout_inline() {
    // verbose is a full transcript: a passing test's own stdout prints
    // right under its `ok` line, in order, instead of being discarded or
    // racing directly onto the real stdout.
    wado_test()
        .args([
            "--format",
            "verbose",
            "wado-cli/tests/fixtures/test_stdout.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok   prints and passes"))
        .stdout(predicate::str::contains("stdout:"))
        .stdout(predicate::str::contains("hello from the test body"));
}

#[test]
fn test_test_report_fuel_joins_the_duration() {
    wado_test()
        .args([
            "--format",
            "verbose",
            "--report-fuel",
            "wado-cli/tests/fixtures/test_stdout.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"ok   prints and passes \(\S+, fuel \d+\)").unwrap());
}

/// The digest stays quiet about a passing test, but not about a reading the
/// run asked for.
#[test]
fn test_test_heartbeat_shows_a_passing_tests_fuel() {
    wado_test()
        .args(["--report-fuel", "wado-cli/tests/fixtures/test_stdout.wado"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(r"ok +\S+ :: prints and passes \(\S+, fuel \d+\)").unwrap(),
        );
}

#[test]
fn test_test_tap_renders_captured_output_as_comments_or_yaml_fields() {
    // TAP allows arbitrary `#` comments, so a passing test's captured
    // stdout renders as one; a failing test's captured stderr instead
    // becomes a `stderr:` field in the same YAML block as `message:`,
    // since that's the one place TAP already gathers rich diagnostics.
    wado_test()
        .args([
            "--format",
            "tap",
            "wado-cli/tests/fixtures/test_stdout.wado",
            "wado-cli/tests/fixtures/test_fail.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("# stdout:"))
        .stdout(predicate::str::contains("# hello from the test body"))
        .stdout(predicate::str::contains("stderr: |"))
        .stdout(predicate::str::contains("this should fail"));
}

#[test]
fn test_test_heartbeat_reports_skip_for_files_without_test_blocks() {
    // A file with zero `test` blocks still compiles and loads; the
    // `skip` axis (not silently folded into `load: N ok`) is how a
    // developer notices "this file has no tests" rather than assuming
    // it was covered.
    wado_test()
        .args([
            "wado-cli/tests/fixtures/test_no_test_blocks.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "skip:    1 files (no test blocks)",
        ))
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_tap_format_produces_a_tap14_document() {
    // `--format tap`: a leading version + plan (file count, known
    // upfront), one top-level Test Point per file — `# SKIP` for a
    // file with no `test` blocks, a `# Subtest:` block (with its own
    // leading plan) for a file with tests, and diagnostics as a YAML
    // block under a failing Test Point.
    wado_test()
        .args([
            "--format",
            "tap",
            "wado-cli/tests/fixtures/test_no_test_blocks.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
            "wado-cli/tests/fixtures/test_fail.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("TAP version 14"))
        .stdout(predicate::str::contains("1..3"))
        .stdout(predicate::str::contains(
            "ok - wado-cli/tests/fixtures/test_no_test_blocks.wado # SKIP no test blocks",
        ))
        .stdout(predicate::str::contains(
            "# Subtest: wado-compiler/tests/fixtures/test_decl.wado",
        ))
        .stdout(predicate::str::contains(
            "not ok - wado-cli/tests/fixtures/test_fail.wado",
        ))
        .stdout(predicate::str::contains("      ---"))
        .stdout(predicate::str::contains("      message: |"))
        // The final summary carries the same compile/load/execute [cpu: …]
        // timing as `verbose`, as `#` comments (TAP allows arbitrary data
        // there) — not just the pass/fail counts.
        .stdout(predicate::str::contains("compile: 3 ok, 0 failed  [cpu:"));
}

#[test]
fn test_test_filter_keeps_matching_path() {
    // --filter is a path-based wildcard. `*` does not cross path
    // separators (consistent with shell glob and `.gitignore`); use `**`
    // to match across directories.
    wado_test()
        .args([
            "--filter",
            "**/test_decl.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_filter_repeatable_keeps_union_of_matches() {
    // `--filter` is repeatable: a path is kept when any pattern matches.
    wado_test()
        .args([
            "--filter",
            "**/test_decl.wado",
            "--filter",
            "**/never_matches.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_compile_failure_reported_on_compile_axis() {
    // A file that fails to compile must (a) not abort the run, (b) be reported
    // on the `compile` axis, and (c) cause a non-zero exit. The peer file
    // still compiles and its passing test still runs.
    wado_test()
        .args([
            "--format",
            "verbose",
            "wado-cli/tests/fixtures/test_compile_error.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("compile: 1 ok, 1 failed"))
        .stdout(predicate::str::contains("test:    2 passed, 0 failed"))
        .stdout(predicate::str::contains("compile failures:"))
        .stdout(predicate::str::contains(
            "wado-cli/tests/fixtures/test_compile_error.wado",
        ))
        .stdout(predicate::str::contains("type mismatch"));
}

#[test]
fn test_test_heartbeat_recaps_compile_failures_at_end() {
    wado_test()
        .args([
            "wado-cli/tests/fixtures/test_compile_error.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("compile: 1 ok, 1 failed"))
        .stdout(predicate::str::contains("compile failures:"))
        .stdout(predicate::str::contains(
            "wado-cli/tests/fixtures/test_compile_error.wado",
        ))
        .stdout(predicate::str::contains("type mismatch"));
}

#[test]
fn test_test_tap_recaps_compile_failures_at_end() {
    wado_test()
        .args([
            "--format",
            "tap",
            "wado-cli/tests/fixtures/test_compile_error.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_match(r"#.*compile: 1 ok, 1 failed").unwrap())
        .stdout(predicate::str::is_match(r"#.*compile failures:").unwrap())
        .stdout(predicate::str::contains(
            "wado-cli/tests/fixtures/test_compile_error.wado",
        ))
        .stdout(predicate::str::contains("type mismatch"));
}

#[test]
fn test_test_heartbeat_recaps_failed_tests_at_end() {
    wado_test()
        .args([
            "wado-cli/tests/fixtures/test_fail.wado",
            "wado-cli/tests/fixtures/test_todo_unexpected_pass.wado",
        ])
        .assert()
        .failure()
        .stdout(
            predicate::str::is_match(
                r"(?s)test:    1 passed, 1 failed.*\ntest failures:\n    wado-cli/tests/fixtures/test_fail\.wado :: failing\n\nunexpectedly passed TODO tests:\n    wado-cli/tests/fixtures/test_todo_unexpected_pass\.wado :: passes although marked\n",
            )
            .unwrap(),
        );
}

#[test]
fn test_test_tap_recaps_failed_tests_at_end() {
    wado_test()
        .args([
            "--format",
            "tap",
            "wado-cli/tests/fixtures/test_fail.wado",
            "wado-cli/tests/fixtures/test_todo_unexpected_pass.wado",
        ])
        .assert()
        .failure()
        .stdout(
            predicate::str::is_match(
                r"(?s)# \S+ test:    1 passed, 1 failed.*\n# \S+ test failures:\n# \S+     wado-cli/tests/fixtures/test_fail\.wado :: failing\n# \S+ unexpectedly passed TODO tests:\n# \S+     wado-cli/tests/fixtures/test_todo_unexpected_pass\.wado :: passes although marked\n",
            )
            .unwrap(),
        );
}

#[test]
fn test_test_no_run_skips_phase_two() {
    // `--no-run` must (a) compile the file successfully — Phase 1 still
    // runs, which is where Kiln caches get refreshed; (b) skip the
    // wasmtime execution phase so the two tests in `test_decl.wado` do
    // not run; (c) exit 0. The compile axis shows 1 ok and the test
    // axis shows zero passes.
    wado_test()
        .args(["--no-run", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("compile: 1 ok, 0 failed"))
        .stdout(predicate::str::contains("test:    0 passed, 0 failed"));
}

#[test]
fn test_test_no_run_surfaces_todo_compile_errors() {
    // A `#![TODO]` module whose expected compile error fires is
    // module-level TODO-pending. Under `--no-run` we can't observe
    // test-level TODO resolution, but the module-level outcome is
    // detectable at compile time and must stay visible — both as a
    // `todo: N pending` summary line and in the consolidated
    // `TODO tests:` section, matching the normal-run path's parity.
    wado_test()
        .args([
            "--no-run",
            "--format",
            "verbose",
            "wado-compiler/tests/fixtures/module_attr_todo_compile_error.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("compile: 1 ok, 0 failed"))
        .stdout(predicate::str::contains("todo:    1 pending"))
        .stdout(predicate::str::contains(
            "wado-compiler/tests/fixtures/module_attr_todo_compile_error.wado",
        ))
        .stdout(predicate::str::contains("#![TODO] module"));
}

#[test]
fn test_test_filter_drops_non_matching_path() {
    // No path matches the pattern, so no test files remain to run.
    wado_test()
        .args([
            "--filter",
            "**/does_not_match.wado",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("No .wado files match --filter"));
}

#[test]
fn test_test_parallel_option() {
    // Test with explicit parallel count
    wado_test()
        .args(["-p", "2", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_parallel_long_option() {
    // Test with long option
    wado_test()
        .args([
            "--parallel",
            "1",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_lib_facade_submodule_export_with_nested_records() {
    // A library's public API lives in a submodule as `export fn`, surfaced by a
    // `pub use` facade in the entry module. The `--lib` build must lower it as a
    // real CM export — including a return type that nests records inside a list,
    // which the lift/lower must resolve to the submodule-defined struct rather
    // than an i32 handle.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"doc\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub use { render, Doc, Node } from \"./render.wado\";\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("render.wado"),
        "export struct Node { pub level: u8, pub text: String }\n\
         export struct Doc { pub html: String, pub nodes: List<Node> }\n\
         export fn render(s: String) -> Doc { return Doc { html: s, nodes: [Node { level: 1, text: s }] }; }\n",
    )
    .unwrap();

    // WIT surfaces the facade export, grouped into the package interface with
    // both records and the nested `list<node>`.
    wado_in(dir)
        .arg("wit")
        .arg("--lib")
        .assert()
        .success()
        .stdout(predicate::str::contains("interface doc {"))
        .stdout(predicate::str::contains("list<node>"))
        .stdout(predicate::str::contains("render: func(s: string) -> doc;"));

    // The build must reach codegen: the export adapter calls the submodule
    // function and lowers the nested list-of-records return without panicking.
    wado_in(dir).arg("build").arg("--lib").assert().success();
}

#[test]
fn test_lib_without_exports_is_rejected() {
    // A `--lib` with neither an `export fn` nor a public type exports nothing;
    // reject it rather than emit an empty library.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"empty\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub fn helper() -> i32 { return 0; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains("exports nothing"));
}

#[test]
fn test_lib_with_only_public_types_is_accepted() {
    // A data-model library (like `jade`) exposes only public types and no
    // `export fn`. Its component surface is those types, so `--lib` must build
    // it, not reject it as empty.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"model\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub struct Point { pub x: i32, pub y: i32 }\n",
    )
    .unwrap();

    wado_in(dir).arg("build").arg("--lib").assert().success();
}

const PUBLISHABLE_HEADER: &str = "[package]\nnamespace = \"acme\"\nname = \"pkg\"\nversion = \"0.1.0\"\ndescription = \"d\"\nrepository = \"https://x/y\"\nlicense = \"MIT\"\nauthors = [\"A\"]\nlib = \"src/lib.wado\"\n\n[registries]\ndefault = \"oci://ghcr.io\"\n";

#[test]
fn test_publish_dry_run_builds_without_pushing() {
    // `--dry-run` builds each world (proving it still compiles into a component)
    // but skips the OCI push, so it succeeds without wkg or credentials.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("wado.toml"), PUBLISHABLE_HEADER).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "export fn ping() -> i32 { return 1; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("publish")
        .arg("--dry-run")
        .assert()
        .success()
        .stderr(predicate::str::contains("dry-run"));
}

#[test]
fn test_publish_dry_run_catches_unpublishable_component() {
    // The dry run builds, so a `--lib` that exports nothing fails it — the
    // release-time failure the CI check is meant to surface at PR time.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("wado.toml"), PUBLISHABLE_HEADER).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub fn helper() -> i32 { return 0; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("publish")
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("exports nothing"));
}

#[test]
fn test_publish_dry_run_reports_every_broken_world() {
    // The worlds build together, so a broken member does not hide another.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado-workspace.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.package]\nversion = \"0.1.0\"\nrepository = \"https://x/y\"\nnamespace = \"acme\"\nlicense = \"MIT\"\nauthors = [\"A\"]\n\n[registries]\ndefault = \"oci://ghcr.io\"\n",
    )
    .unwrap();
    for member in ["a", "b"] {
        let src = dir.join(member).join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join(member).join("wado.toml"),
            format!(
                "[package]\nname = \"{member}\"\ndescription = \"d\"\nlib = \"src/lib.wado\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("lib.wado"),
            "pub fn helper() -> i32 { return 0; }\n",
        )
        .unwrap();
    }

    wado_in(dir)
        .arg("publish")
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("exports nothing").count(2));
}

/// Run from a directory of a workspace that is no member, publish names the
/// workspace root to run from.
#[test]
fn test_publish_outside_a_member_names_the_workspace_root() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado-workspace.toml"),
        "[workspace]\nmembers = [\"a\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("tools")).unwrap();

    wado_in(&dir.join("tools"))
        .arg("publish")
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "run `wado publish` from its root:\n  {}",
            dir.canonicalize().unwrap().display()
        )));
}

#[test]
fn test_lib_duplicate_export_name_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"dup\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub use { f } from \"./a.wado\";\npub use { g } from \"./b.wado\";\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("a.wado"),
        "export fn f(s: String) -> String { return s; }\nexport fn dup(s: String) -> String { return s; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("b.wado"),
        "export fn g(s: String) -> String { return s; }\nexport fn dup(s: String) -> String { return s; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains("two functions named `dup`"));
}

/// An `internal` type is published into the CM interface so an export naming it
/// resolves, but it is not the library's API, so it cannot be what a `--lib`
/// offers its consumers.
#[test]
fn test_lib_with_only_internal_submodule_types_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"inner\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("geom.wado"),
        "internal struct Point { internal x: i32 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "use { Point } from \"./geom.wado\";\npub fn helper() -> i32 { return 0; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains("exports nothing"));
}

/// An `internal` type can never be published, since `export` implies `pub` and
/// `export use` cannot widen, so one a published type holds is refused.
#[test]
fn test_lib_refuses_an_internal_type_held_by_a_published_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"vis\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("geom.wado"),
        "internal struct Point { internal x: i32, internal y: i32 }\n\
         export struct Marker { pub at: Point }\n\
         impl Marker {\n\
         pub fn new() -> Marker { return Marker { at: Point { x: 0, y: 0 } }; }\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "use { Marker } from \"./geom.wado\";\n\
         export fn origin() -> Marker { return Marker::new(); }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`Point` in return type of `origin` is not published",
        ));
}

#[test]
fn test_lib_duplicate_type_name_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"dup\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.wado"),
        "pub use { f } from \"./a.wado\";\npub use { g } from \"./b.wado\";\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("a.wado"),
        "export struct Node { pub v: String }\nexport fn f(s: String) -> Node { return Node { v: s }; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("b.wado"),
        "export struct Node { pub v: String }\nexport fn g(s: String) -> Node { return Node { v: s }; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "type `Node` is defined in ./a.wado and ./b.wado",
        ));
}
