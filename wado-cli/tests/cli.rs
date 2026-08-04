//! End-to-end tests for wado CLI
//!
//! Tests the CLI interface including argument parsing, subcommands,
//! and integration with the compiler.

use predicates::prelude::*;

mod common;
use common::{custom_sections, wado, wado_in};

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
        "pub struct Point { x: f64, y: f64 }\nexport fn identity(p: Point) -> Point { return p; }\n",
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
        "pub struct Point { x: f64, y: f64 }\nexport fn identity(p: Point) -> Point { return p; }\n",
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
fn test_check_world_default_requires_run() {
    // Without `--world test`, `wado check` targets `wasi:cli/command` and the
    // missing `run` entry point makes the check fail.
    wado()
        .args(["check", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));
}

#[test]
fn test_run_hello() {
    wado()
        .args(["run", "example/hello.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, world!"));
}

#[test]
fn test_test_passing() {
    wado()
        .args(["test", "wado-compiler/tests/fixtures/test_decl.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_failing() {
    wado()
        .args(["test", "wado-cli/tests/fixtures/test_fail.wado"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("1 passed, 1 failed"));
}

#[test]
fn test_test_heartbeat_is_default_and_reports_failure_immediately() {
    // `heartbeat` is the default `--format`: no per-file `Compiled`/`Loaded`
    // log lines and no per-test `ok`/`FAILED` lines (that's `verbose`
    // territory) — but a failing test still gets its own `not ok` line,
    // not just a buried count in the final summary.
    wado()
        .args(["test", "wado-cli/tests/fixtures/test_fail.wado"])
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
    wado()
        .args(["test", "wado-cli/tests/fixtures/test_stdout.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from the test body").not());
}

#[test]
fn test_test_heartbeat_shows_captured_stderr_on_failure() {
    // A failing test's captured stderr is attached to its immediate
    // failure notice — not just discarded like a passing test's output.
    wado()
        .args(["test", "wado-cli/tests/fixtures/test_fail.wado"])
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
    wado()
        .args([
            "test",
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
fn test_test_tap_renders_captured_output_as_comments_or_yaml_fields() {
    // TAP allows arbitrary `#` comments, so a passing test's captured
    // stdout renders as one; a failing test's captured stderr instead
    // becomes a `stderr:` field in the same YAML block as `message:`,
    // since that's the one place TAP already gathers rich diagnostics.
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
fn test_test_no_run_skips_phase_two() {
    // `--no-run` must (a) compile the file successfully — Phase 1 still
    // runs, which is where Kiln caches get refreshed; (b) skip the
    // wasmtime execution phase so the two tests in `test_decl.wado` do
    // not run; (c) exit 0. The compile axis shows 1 ok and the test
    // axis shows zero passes.
    wado()
        .args([
            "test",
            "--no-run",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
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
    wado()
        .args([
            "test",
            "-p",
            "2",
            "wado-compiler/tests/fixtures/test_decl.wado",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
}

#[test]
fn test_test_parallel_long_option() {
    // Test with long option
    wado()
        .args([
            "test",
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
        "pub struct Node { pub level: u8, pub text: String }\n\
         pub struct Doc { pub html: String, pub nodes: List<Node> }\n\
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
        "pub struct Node { pub v: String }\nexport fn f(s: String) -> Node { return Node { v: s }; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("b.wado"),
        "pub struct Node { pub v: String }\nexport fn g(s: String) -> Node { return Node { v: s }; }\n",
    )
    .unwrap();

    wado_in(dir)
        .arg("build")
        .arg("--lib")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "library type `Node` is defined in more than one module",
        ));
}
