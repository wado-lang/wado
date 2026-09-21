//! A generator that uses a generator, end to end: `wado run` over a project
//! whose generator's own source imports a module Kiln produces.
//!
//! See WEP 2026-04-12 §"A generator may use a generator".

use std::path::Path;

use crate::common::{project_root, wado_in};

/// A fixture project copied out of the tree, so a test may edit its sources
/// and so no build output lands beside them.
fn fixture(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(
        &project_root().join("wado-cli/tests/fixtures").join(name),
        tmp.path(),
    );
    tmp
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn run_project(root: &Path) -> std::process::Output {
    wado_in(root)
        .args(["run", "src/main.wado"])
        .output()
        .expect("wado run")
}

#[test]
fn a_generator_importing_a_generated_module_runs() {
    let project = fixture("kiln_nested");

    let out = run_project(project.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a generator whose source imports a generated module must build:\n{}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 42"),
        "the innermost input must reach the program's output, got: {stdout}"
    );
}

#[test]
fn editing_the_innermost_input_rebuilds_the_generator() {
    let project = fixture("kiln_nested");
    assert!(run_project(project.path()).status.success(), "first build");

    // The generated module is an input to the outer generator, so what it was
    // generated from reaches the outer generator's identity.
    std::fs::write(project.path().join("src/value.txt"), "7\n").unwrap();

    let out = run_project(project.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "rebuild after editing the innermost input:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 7"),
        "editing what the inner generator reads must rebuild the outer one, got: {stdout}"
    );
}

/// A generator's own clause may name a `[build-dependencies]` nickname, whose
/// path is spelled against the project root while the clause resolves against
/// the generator's own directory.
#[test]
fn a_nested_clause_resolves_a_build_dependency_nickname() {
    let project = fixture("kiln_nested_build_dep");

    let out = run_project(project.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a nested `lib:` specifier must resolve:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 99"),
        "the nickname-resolved generator must produce the value, got: {stdout}"
    );
}

/// A generator shipped as its own package names its dependencies in its own
/// manifest, so a nested clause resolves against the generator's project rather
/// than the consumer's. The consumer here declares no build-dependency at all.
#[test]
fn a_nested_clause_resolves_against_the_generators_own_package() {
    let project = fixture("kiln_nested_own_package");

    let out = run_project(project.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a generator's own `[build-dependencies]` must resolve:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("model 7"),
        "the generator's own dependency must produce the value, got: {stdout}"
    );
}

/// A second anchor would give one schema two identities and two output trees,
/// and a generated file only one of the two routes can satisfy.
#[test]
fn a_nested_invocation_writes_into_the_one_package_tree() {
    let project = fixture("kiln_nested");
    assert!(run_project(project.path()).status.success(), "first build");

    let mut trees = Vec::new();
    collect_kiln_trees(project.path(), project.path(), &mut trees);
    assert_eq!(trees, ["build/kiln"], "one anchor, one tree");
}

fn collect_kiln_trees(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        if path.ends_with("build/kiln") {
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            continue;
        }
        collect_kiln_trees(root, &path, out);
    }
}

/// `wado check` runs the pipeline and writes, as a build does, so it works on a
/// tree that has never been built. A route that only compared would report
/// every generated file as missing and then fail to read the one it needs.
#[test]
fn check_materializes_a_nested_generated_file_on_an_unbuilt_tree() {
    let project = fixture("kiln_nested");

    wado_in(project.path())
        .args(["check", "src/main.wado"])
        .assert()
        .success();

    let generated = std::fs::read_dir(project.path().join("build/kiln"))
        .expect("check wrote the kiln tree")
        .find_map(|e| std::fs::read_to_string(e.ok()?.path().join("value.wado")).ok())
        .expect("value.wado among the nested outputs");
    assert!(
        generated.contains("42"),
        "the innermost input must reach the generated module, got: {generated}"
    );
}

/// An edited input regenerates rather than failing: the overwrite is what the
/// edit asked for, so only an integrity finding gates the check.
#[test]
fn check_regenerates_after_an_edit_to_the_innermost_input() {
    let project = fixture("kiln_nested");
    assert!(run_project(project.path()).status.success(), "first build");

    std::fs::write(project.path().join("src/value.txt"), "7\n").unwrap();

    wado_in(project.path())
        .args(["check", "src/main.wado"])
        .assert()
        .success();
}

#[test]
fn a_generator_cycle_is_reported_naming_the_invocations() {
    let project = fixture("kiln_nested_cycle");

    let out = run_project(project.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a generator cycle must stop the build"
    );
    assert!(
        stderr.contains("cycle") && stderr.contains("cyclic.wado"),
        "the cycle must be reported naming the generator in it, got: {stderr}"
    );
}
