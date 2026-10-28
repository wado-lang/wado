//! End-to-end: `wado check` over a package fails on a Kiln warning wherever
//! the member raising it sits, a test file as much as an entry.

use crate::common::{wado_in, write_kiln_project};
use predicates::prelude::*;

const GREETING_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    return Result::Ok(Response {
        files: [OutputFile {
            path: "hello.wado",
            content: "pub fn hello() -> i32 { return 1; }\n",
            is_entry: true,
        }],
    });
}
"#;

#[test]
fn a_kiln_warning_in_a_test_member_fails_the_package_check() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");
    std::fs::rename(root.join("src/main.wado"), root.join("src/hello_test.wado")).unwrap();
    std::fs::write(
        root.join("src/hello_test.wado"),
        "use { hello } from \"./schema.bin\"\n    with { generator: { module: \"./gen.wado\" } };\n\n\
         test { assert hello() == 1; }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.wado"), "export fn run() {}\n").unwrap();

    wado_in(root).arg("check").assert().success();
    let generated = walkdir(root)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "hello.wado"))
        .expect("the generator wrote hello.wado");
    std::fs::write(&generated, "pub fn hello() -> i32 { return 2; }\n").unwrap();

    wado_in(root)
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "has been modified after generation",
        ));
}

/// The package pass reuses what an entry's own check ran, so it neither runs
/// the entry's generators again nor reports their warnings twice.
#[test]
fn a_package_check_reports_an_entry_kiln_warning_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");

    wado_in(root).arg("check").assert().success();
    let generated = walkdir(root)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "hello.wado"))
        .expect("the generator wrote hello.wado");
    std::fs::write(&generated, "pub fn hello() -> i32 { return 2; }\n").unwrap();

    wado_in(root)
        .args(["check", "--warn"])
        .assert()
        .success()
        .stderr(predicate::function(|stderr: &str| {
            stderr.matches("has been modified after generation").count() == 1
        }));
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// A generator clause in a module several members reach runs and warns once,
/// not once per member.
#[test]
fn a_package_check_reports_a_shared_module_kiln_warning_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");
    std::fs::write(
        root.join("src/shared.wado"),
        "use { hello } from \"./schema.bin\"\n    with { generator: { module: \"./gen.wado\" } };\n\n\
         pub fn shared() -> i32 { return hello(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.wado"),
        "use { shared } from \"./shared.wado\";\n\nexport fn run() { shared(); }\n",
    )
    .unwrap();
    for name in ["a_test", "b_test"] {
        std::fs::write(
            root.join(format!("src/{name}.wado")),
            "use { shared } from \"./shared.wado\";\n\ntest { assert shared() == 1; }\n",
        )
        .unwrap();
    }

    wado_in(root).arg("check").assert().success();
    let generated = walkdir(root)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "hello.wado"))
        .expect("the generator wrote hello.wado");
    std::fs::write(&generated, "pub fn hello() -> i32 { return 2; }\n").unwrap();

    wado_in(root)
        .args(["check", "--warn"])
        .assert()
        .success()
        .stderr(predicate::function(|stderr: &str| {
            stderr.matches("has been modified after generation").count() == 1
        }));
}

/// Compiling a generator to run it lints nothing: the generator's own package
/// check is where its lints belong, and reports them once. `generate` is an
/// `export`, so the component boundary names its parameter and leaving it
/// unread is no defect.
#[test]
fn compiling_a_generator_lints_none_of_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let generator = GREETING_GENERATOR.replace(
        "    return Result::Ok(",
        "    let unread_local = 1;\n    return Result::Ok(",
    );
    write_kiln_project(root, "pkg", &generator, b"schema\n");

    wado_in(root)
        .arg("check")
        .assert()
        .success()
        .stderr(predicate::str::contains("[kiln generator compile]").not())
        .stderr(predicate::str::contains("parameter `req`").not())
        .stderr(predicate::function(|stderr: &str| {
            stderr
                .matches("variable `unread_local` is never used")
                .count()
                == 1
        }));
}

/// A member in a subdirectory reaches a sibling directory's module through
/// `../`, and the package pass places that module in the package as the
/// member's own check does, so their clauses on one schema are one invocation.
#[test]
fn a_member_reaches_a_sibling_directory_within_the_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");
    std::fs::write(root.join("src/main.wado"), "export fn run() {}\n").unwrap();
    for dir in ["tools", "tests"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("tools/shared.wado"),
        "use g from \"../src/schema.bin\"\n    \
         with { generator: { module: \"../src/gen.wado\", output_dir: \"../generated\" } };\n\n\
         pub fn shared() -> i32 { return g::hello(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/shared_test.wado"),
        "use g from \"../src/schema.bin\"\n    \
         with { generator: { module: \"../src/gen.wado\", output_dir: \"../generated\" } };\n\
         use { shared } from \"../tools/shared.wado\";\n\n\
         test { assert shared() == g::hello(); }\n",
    )
    .unwrap();

    wado_in(root).arg("check").assert().success();
}

/// Output a generator writes inside the package is the importing module's, not
/// a member of its own, so the package pass loads it once.
#[test]
fn generated_output_inside_the_package_is_no_member() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_kiln_project(root, "pkg", GREETING_GENERATOR, b"schema\n");
    std::fs::write(
        root.join("src/main.wado"),
        "use { hello } from \"./schema.bin\"\n    \
         with { generator: { module: \"./gen.wado\", output_dir: \"./generated\" } };\n\n\
         export fn run() { hello(); }\n",
    )
    .unwrap();

    wado_in(root).arg("check").assert().success();
    assert!(root.join("src/generated/hello.wado").exists());
    wado_in(root)
        .args(["check", "--lint-generated"])
        .assert()
        .success()
        .stderr(predicate::str::contains("never used").not());
}
