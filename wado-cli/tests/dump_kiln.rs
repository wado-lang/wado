//! Regression test: `wado dump` must run the Kiln generator pipeline like
//! `wado run`/`wado compile`/`wado check` do.
//!
//! `dump_with_host_and_world` builds its own `ModuleLoader` without ever
//! wiring in the invocations index the Kiln pipeline produces, so a `use x
//! from "./schema.ext" with { generator: ... }` clause fell through to the
//! plain module loader, which read the *schema* file (not Wado source, e.g.
//! a `.g4` grammar) directly and fed it to the Wado lexer.

use std::fs;

mod common;
use common::wado_in;
use predicates::prelude::*;

/// Ignores the schema content and always emits the same fixed Wado module —
/// close to how a real generator (e.g. Gale) synthesizes Wado source from a
/// grammar that is not itself valid Wado.
const FIXED_OUTPUT_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.content;
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: "pub fn hello() -> i32 { return 1; }\n",
            is_entry: true,
        }],
    });
}
"#;

fn write_project(root: &std::path::Path) {
    fs::write(root.join("gen.wado"), FIXED_OUTPUT_GENERATOR).unwrap();
    // Not valid Wado source: a multi-character literal, as a real `.g4`
    // grammar's alternative (`expr '||' expr`) would lex if fed to the
    // plain Wado module loader instead of being routed through Kiln.
    fs::write(root.join("grammar.g4"), "expr : expr '||' expr ;\n").unwrap();
    fs::write(
        root.join("entry.wado"),
        "use { println, Stdout } from \"core:cli\";\n\
         use { hello } from \"./grammar.g4\"\n    with { generator: { module: \"./gen.wado\" } };\n\n\
         export fn run() with Stdout {\n    println(`{hello()}`);\n}\n",
    )
    .unwrap();
}

#[test]
fn dump_modules_runs_kiln_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    // Sanity check: `run` and `check` already route this through Kiln.
    wado_in(root)
        .args(["run", "entry.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));

    // The regression: `dump` must do the same instead of feeding the raw
    // (non-Wado) `grammar.g4` text to the Wado lexer.
    wado_in(root)
        .args(["dump", "--modules", "entry.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gen.wado").or(predicate::str::contains("out.wado")));
}

#[test]
fn dump_wir_runs_kiln_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root);

    // `-O0` so `hello()` (a constant return) survives inlining/DCE and its
    // name is still observable in the dumped WIR.
    wado_in(root)
        .args(["dump", "-O0", "entry.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}
