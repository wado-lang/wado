//! `wado test --pgo`: which tests it runs, where the profile goes, how it names
//! modules, and when it writes nothing. Then a build reading it back under
//! `-f pgo`. The profile's format is `wado_compiler::pgo`'s to test.

use std::fs;
use std::path::Path;

use predicates::prelude::*;

use crate::common::wado_in;

const PICK: &str =
    "pub fn pick(x: i32) -> i32 {\n    if x > 0 {\n        return 2;\n    }\n    return 0;\n}\n";

/// A package whose workload calls `pick` five times, once with `0`, beside a
/// test that is no workload and a file with no workload at all.
fn write_package(root: &Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("lib.wado"), PICK).unwrap();
    fs::write(
        root.join("bench_test.wado"),
        "use { pick } from \"./lib.wado\";\n\n\
         #[workload]\ntest \"warm\" {\n    for let i of 0..<5 {\n        assert pick(i) >= 0;\n    }\n}\n\n\
         test \"plain\" {\n    assert pick(-1) == 0;\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("other_test.wado"),
        "use { pick } from \"./lib.wado\";\n\ntest \"other\" {\n    assert pick(-2) == 0;\n}\n",
    )
    .unwrap();
}

/// The lines of `module`'s section of `profile`, up to the next module.
fn section<'a>(profile: &'a str, module: &str) -> Vec<&'a str> {
    profile
        .lines()
        .skip_while(|line| *line != format!("module {module}"))
        .skip(1)
        .take_while(|line| !line.starts_with("module "))
        .collect()
}

#[test]
fn only_the_workloads_are_recorded_into_the_package_profile() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pgo: wrote pgo/default.pgo"));

    let profile = fs::read_to_string(tmp.path().join("pgo/default.pgo")).unwrap();
    assert!(profile.starts_with("wado-pgo 1\n"), "{profile}");
    assert!(profile.contains("\nmodule core:prelude\n"), "{profile}");
    let lib = section(&profile, "acme:app@0.1.0/lib.wado");
    assert_eq!(lib.len(), 5, "{profile}");
    assert!(
        lib[0].starts_with("fn ") && lib[0].ends_with(" 1 pick"),
        "{profile}"
    );
    // `plain` and `other` are no workloads, so the else branch ran once.
    assert_eq!(lib[1..], ["0 5", "1 4", "2 1", "3 1"], "{profile}");

    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .success();
    let again = fs::read_to_string(tmp.path().join("pgo/default.pgo")).unwrap();
    assert_eq!(
        again, profile,
        "a deterministic workload records the same profile"
    );
}

#[test]
fn a_program_in_no_package_keeps_a_profile_of_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("lib.wado"), PICK).unwrap();
    fs::write(
        tmp.path().join("prog.wado"),
        "use { pick } from \"./lib.wado\";\n\n#[workload]\ntest {\n    assert pick(1) == 2;\n}\n",
    )
    .unwrap();

    wado_in(tmp.path())
        .args(["test", "--pgo", "prog.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pgo: wrote pgo/prog.pgo"));
    let profile = fs::read_to_string(tmp.path().join("pgo/prog.pgo")).unwrap();
    assert!(profile.contains("\nmodule lib.wado\n"), "{profile}");
    assert!(profile.contains("\nmodule prog.wado\n"), "{profile}");
}

#[test]
fn a_dependency_is_named_by_its_package() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep");
    fs::create_dir_all(dep.join("src")).unwrap();
    fs::write(
        dep.join("wado.toml"),
        "[package]\nnamespace = \"acme\"\nname = \"dep\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
    )
    .unwrap();
    fs::write(dep.join("src/lib.wado"), PICK).unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\n\"lib:dep\" = { path = \"../dep\" }\n",
    )
    .unwrap();
    fs::write(
        app.join("bench_test.wado"),
        "use { pick } from \"lib:dep\";\n\n#[workload]\ntest {\n    assert pick(1) == 2;\n}\n",
    )
    .unwrap();

    wado_in(&app).args(["test", "--pgo"]).assert().success();
    let profile = fs::read_to_string(app.join("pgo/default.pgo")).unwrap();
    assert!(
        profile.contains("\nmodule acme:dep@0.1.0/src/lib.wado\n"),
        "{profile}"
    );
    assert!(
        profile.contains("\nmodule app@0.1.0/bench_test.wado\n"),
        "{profile}"
    );
}

#[test]
fn a_run_with_nothing_sound_to_record_keeps_the_profile() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());
    fs::create_dir_all(tmp.path().join("pgo")).unwrap();
    let profile = tmp.path().join("pgo/default.pgo");
    fs::write(&profile, "kept\n").unwrap();

    fs::write(
        tmp.path().join("bench_test.wado"),
        "#[workload]\ntest \"breaks\" {\n    assert 1 == 2;\n}\n",
    )
    .unwrap();
    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "pgo: the run failed, so no profile was written",
        ));
    assert_eq!(fs::read_to_string(&profile).unwrap(), "kept\n");

    // A pending TODO does not fail the run, but its workload trapped all the same.
    fs::write(
        tmp.path().join("bench_test.wado"),
        "#![TODO]\n\n#[workload]\ntest \"pending\" {\n    assert 1 == 2;\n}\n",
    )
    .unwrap();
    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "pgo: a `#[TODO]` workload or file is pending, so no profile was written",
        ));
    assert_eq!(fs::read_to_string(&profile).unwrap(), "kept\n");

    fs::remove_file(tmp.path().join("bench_test.wado")).unwrap();
    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "pgo: no `#[workload]` test ran, so no profile was written",
        ));
    assert_eq!(fs::read_to_string(&profile).unwrap(), "kept\n");
}

#[test]
fn a_package_left_with_no_workload_loses_its_profile() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        let root = tmp.path().join(name);
        fs::create_dir_all(&root).unwrap();
        write_package(&root);
    }

    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .success();
    let stale = tmp.path().join("b/pgo/default.pgo");
    assert!(stale.exists());

    fs::remove_file(tmp.path().join("b/bench_test.wado")).unwrap();
    wado_in(tmp.path())
        .args(["test", "--pgo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pgo: wrote a/pgo/default.pgo"))
        .stdout(predicate::str::contains("pgo: removed b/pgo/default.pgo"));
    assert!(!stale.exists());
}

/// A build applies the profile `--pgo` recorded unless `-f no-pgo` says not
/// to. The conflict remark on the `cold_path()` the workload ran shows it was
/// applied.
#[test]
fn a_build_applies_the_entry_profile_under_pgo() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("prog.wado"),
        "use { println, Stdout } from \"core:cli\";\n\n\
         fn pick(x: i32) -> i32 {\n    if x > 0 {\n        builtin::cold_path();\n        return 2;\n    }\n    return 0;\n}\n\n\
         #[workload]\ntest {\n    assert pick(1) == 2;\n}\n\n\
         export fn run() with Stdout {\n    println(`${pick(1)}`);\n}\n",
    )
    .unwrap();
    let remark = "`cold_path()` marks a path the profile ran once";
    let missing = "-f pgo: no profile at";
    let run = || {
        wado_in(tmp.path())
            .args(["run", "--log-level", "info", "prog.wado"])
            .assert()
            .success()
            .stdout("2\n")
    };
    let compile = |flags: &[&str]| {
        wado_in(tmp.path())
            .args(["compile", "--log-level", "info", "-o", "out.wasm"])
            .args(flags)
            .arg("prog.wado")
            .assert()
            .success()
    };

    run().stderr(predicate::str::contains(remark).not());
    wado_in(tmp.path())
        .args(["test", "--pgo", "prog.wado"])
        .assert()
        .success();
    // The build cached above depended on the profile's absence.
    run().stderr(predicate::str::contains(remark));
    compile(&["-f", "no-pgo"]).stderr(predicate::str::contains(remark).not());

    fs::remove_file(tmp.path().join("pgo/prog.pgo")).unwrap();
    compile(&["-f", "pgo"]).stderr(predicate::str::contains(missing));
    compile(&[]).stderr(predicate::str::contains(missing).not());
}

#[test]
fn pgo_runs_every_workload_of_a_package() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    for flags in [
        ["--filter", "*bench*"],
        ["--test-name", "warm"],
        ["--exclude", "other_test.wado"],
    ] {
        wado_in(tmp.path())
            .args(["test", "--pgo"])
            .args(flags)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "--pgo records every workload, so it takes no --filter, --test-name or --exclude",
            ));
    }
    wado_in(tmp.path())
        .args(["test", "--pgo", "bench_test.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--pgo records all of a package's workloads into one profile, but this run \
             leaves out lib.wado of the package at",
        ));
    wado_in(tmp.path())
        .args(["test", "--pgo", "--coverage"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--pgo and --coverage each measure the run; pick one",
        ));
    wado_in(tmp.path())
        .args(["test", "--pgo", "--no-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--pgo has nothing to record under --no-run",
        ));
}
