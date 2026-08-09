//! Optimizer remark tests (WEP `wep-2026-06-03-optimizer-remarks.md`).
//!
//! A value-semantic copy that survives the optimizer is reported as an
//! info-level `remark:` diagnostic with a source span; a copy the optimizer
//! removes (scalarizes / elides) is not.

mod common;

use common::{InMemoryHost, runtime};
use wado_compiler::{CompilerOptions, OptLevel};

/// Compile `source` at `-O2` and return the `remark:` diagnostics as
/// `"line:col message"` strings.
fn remarks_for(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        ..CompilerOptions::default()
    };
    let _ = runtime().block_on(wado_compiler::compile_with_options(
        source,
        &host,
        Some("test.wado"),
        options,
    ));
    host.diagnostics()
        .into_iter()
        .filter(|d| d.message.starts_with("remark:"))
        .map(|d| {
            let loc = d
                .span
                .as_ref()
                .map(|s| format!("{}:{}", s.line, s.column))
                .unwrap_or_default();
            format!("{loc} {}", d.message)
        })
        .collect()
}

#[test]
fn surviving_list_copy_is_remarked() {
    // `b` is a deep value-copy of `a` that is then mutated, so the copy of the
    // backing array cannot be elided and survives to the final IR.
    let remarks = remarks_for(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let a: List<i32> = [1, 2, 3];
    let mut b = a;
    b.push(4);
    println(`${a.len()} ${b.len()}`);
}
"#,
    );

    assert_eq!(
        remarks.len(),
        1,
        "expected exactly one remark, got {remarks:?}"
    );
    assert!(
        remarks[0].contains("a copy of") && remarks[0].contains("List<i32>"),
        "unexpected remark text: {remarks:?}"
    );
    // The remark points at the copying statement `let mut b = a;` (line 6).
    assert!(
        remarks[0].starts_with("6:"),
        "remark should point at the copy statement on line 6: {remarks:?}"
    );
}

#[test]
fn scalarized_struct_copy_is_not_remarked() {
    // SROA scalarizes `Point` into i32 locals, so the copy disappears entirely;
    // no heap copy executes, so there is nothing to remark on.
    let remarks = remarks_for(
        r#"
use { println, Stdout } from "core:cli";

struct Point { x: i32, y: i32 }

export fn run() with Stdout {
    let a = Point { x: 1, y: 2 };
    let mut b = a;
    b.x = 9;
    println(`${a.x} ${b.x}`);
}
"#,
    );

    assert!(
        remarks.is_empty(),
        "expected no remark (copy scalarized away), got {remarks:?}"
    );
}

#[test]
fn struct_field_copy_remark_points_at_copy_statement() {
    // `Bag` is SROA-decomposed and its `items` copy is reconstructed inside a
    // synthesized block whose inner statements carry placeholder spans. The
    // remark must anchor to the enclosing real statement `let mut b = a;`
    // (line 8), not to the placeholder span of the inner synthesized statement.
    let remarks = remarks_for(
        r#"
use { println, Stdout } from "core:cli";

struct Bag { items: List<i32> }

export fn run() with Stdout {
    let a = Bag { items: [1, 2, 3] };
    let mut b = a;
    b.items.push(4);
    println(`${a.items.len()} ${b.items.len()}`);
}
"#,
    );

    assert_eq!(
        remarks.len(),
        1,
        "expected exactly one remark, got {remarks:?}"
    );
    assert!(
        remarks[0].contains("a copy of") && remarks[0].contains("List<i32>"),
        "unexpected remark text: {remarks:?}"
    );
    assert!(
        remarks[0].starts_with("8:"),
        "remark should point at the copy statement on line 8, not a placeholder span: {remarks:?}"
    );
}

/// Compile `source` at `-O2` with `-D` overrides and return the `remark:`
/// diagnostics as `"line:col message"` strings.
fn remarks_for_params(source: &str, overrides: &[(&str, &str)]) -> Vec<String> {
    let host = InMemoryHost::new();
    let mut param_overrides = wado_compiler::hashmap::IndexMap::default();
    for (k, v) in overrides {
        param_overrides.insert((*k).to_string(), (*v).to_string());
    }
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        param_overrides,
        ..CompilerOptions::default()
    };
    let _ = runtime().block_on(wado_compiler::compile_with_options(
        source,
        &host,
        Some("test.wado"),
        options,
    ));
    host.diagnostics()
        .into_iter()
        .filter(|d| d.message.starts_with("remark:"))
        .map(|d| {
            let loc = d
                .span
                .as_ref()
                .map(|s| format!("{}:{}", s.line, s.column))
                .unwrap_or_default();
            format!("{loc} {}", d.message)
        })
        .collect()
}

#[test]
fn param_reaching_a_branch_is_remarked() {
    // A constant does not reach a `String` parameter compared with `==`, so the
    // gate survives to run time even though `-D log.level=info` settles it.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: String = "trace";

fn is_trace(s: String) -> bool {
    if s == "trace" {
        return true;
    }
    return false;
}

export fn run() with Stdout {
    if is_trace(LOG_LEVEL) {
        println(`tracing`);
    }
}
"#,
        &[("log.level", "info")],
    );
    assert!(
        remarks
            .iter()
            .any(|r| r.contains("compile-time parameter `log.level` is still read here")),
        "expected a param-gate remark, got {remarks:?}"
    );
}

#[test]
fn param_that_folds_is_not_remarked() {
    // Read directly, the parameter reaches the comparison and the branch is
    // decided at build time — nothing survives to remark on.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: i32 = 0;

export fn run() with Stdout {
    if LOG_LEVEL < 2 {
        println(`tracing`);
    }
}
"#,
        &[("log.level", "3")],
    );
    assert!(
        !remarks.iter().any(|r| r.contains("compile-time parameter")),
        "a folded parameter should not be remarked, got {remarks:?}"
    );
}

#[test]
fn one_gate_reading_a_param_twice_is_remarked_once() {
    // The gate failed once, so it is reported once — however many times the
    // condition reads the parameter.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: String = "trace";

fn is_level(s: String, want: String) -> bool {
    if s == want {
        return true;
    }
    return false;
}

export fn run() with Stdout {
    if is_level(LOG_LEVEL, "trace") || is_level(LOG_LEVEL, "debug") {
        println(`verbose`);
    }
}
"#,
        &[("log.level", "info")],
    );
    let gate_remarks: Vec<&String> = remarks
        .iter()
        .filter(|r| r.contains("compile-time parameter `log.level`"))
        .collect();
    assert_eq!(
        gate_remarks.len(),
        1,
        "one gate should yield one remark, got {remarks:?}"
    );
}

#[test]
fn one_source_gate_inlined_into_two_callers_is_remarked_once() {
    // Inlining copies the gate into every caller, but there is one gate in the
    // source and one line:column to point at.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: String = "trace";

fn is_level(s: String, want: String) -> bool {
    if s == want {
        return true;
    }
    return false;
}

fn gated() -> i32 {
    if is_level(LOG_LEVEL, "trace") {
        return 1;
    }
    return 0;
}

export fn other() -> i32 {
    return gated();
}

export fn run() with Stdout {
    println(`${gated() + other()}`);
}
"#,
        &[("log.level", "info")],
    );
    let gate_remarks: Vec<&String> = remarks
        .iter()
        .filter(|r| r.contains("compile-time parameter `log.level`"))
        .collect();
    assert_eq!(
        gate_remarks.len(),
        1,
        "one source gate should yield one remark, got {remarks:?}"
    );
}

#[test]
fn a_gate_on_a_param_derived_global_is_remarked() {
    // `core:log`'s shape: the gate reads a global derived from the parameter,
    // never the parameter itself, so nothing at the branch names `log.level`.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: String = "trace";

global STATIC_LEVEL: i32 = level_from_str(LOG_LEVEL);

fn level_from_str(s: String) -> i32 {
    if s == "trace" {
        return 0;
    }
    return 2;
}

export fn run() with Stdout {
    if STATIC_LEVEL < 2 {
        println(`tracing`);
    }
}
"#,
        &[("log.level", "info")],
    );
    assert!(
        remarks.iter().any(|r| r.contains(
            "compile-time parameter `log.level` is still read here through global \
             `STATIC_LEVEL`"
        )),
        "expected a remark naming the derived global, got {remarks:?}"
    );
}

#[test]
fn param_used_outside_a_gate_is_not_remarked() {
    // Printing a parameter is an ordinary use of its value. The read survives
    // by design, and the branch it sits under is decided by something else.
    let remarks = remarks_for_params(
        r#"
use { println, Stdout } from "core:cli";

#[param(name = "log.level")]
global LOG_LEVEL: String = "trace";

export fn run(args: List<String>) with Stdout {
    if args.len() > 0 {
        println(LOG_LEVEL);
    }
}
"#,
        &[("log.level", "info")],
    );
    assert!(
        !remarks.iter().any(|r| r.contains("compile-time parameter")),
        "a parameter outside a scrutinee should not be remarked, got {remarks:?}"
    );
}
