//! Optimizer remark tests (WEP `wep-2026-06-03-optimizer-remarks.md`).
//!
//! A value-semantic copy that survives the optimizer is reported as an
//! info-level `remark:` diagnostic with a source span; a copy the optimizer
//! removes (scalarizes / elides) is not.

use crate::common::{InMemoryHost, runtime};
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
    // backing array cannot be elided and survives to the final IR. The list
    // spine scalarizes away, leaving the array itself as what is copied.
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
        remarks[0].contains("a copy of") && remarks[0].contains("Array<i32>"),
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
    // `Bag` is SROA-decomposed and its `items` array copy is reconstructed
    // inside a synthesized block whose inner statements carry placeholder
    // spans. The remark must anchor to the enclosing real statement
    // `let mut b = a;` (line 8), not to the inner statement's placeholder span.
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
        remarks[0].contains("a copy of") && remarks[0].contains("Array<i32>"),
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

/// A `CompilerHost` serving a fixed set of in-memory modules, so a multi-file
/// program can be compiled without touching the filesystem.
struct MultiFileHost {
    files: wado_compiler::hashmap::IndexMap<String, String>,
    diagnostics: std::sync::Mutex<Vec<wado_compiler::Diagnostic>>,
}

impl wado_compiler::CompilerHost for MultiFileHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, wado_compiler::SourceError>> + Send {
        let found = self.files.get(path).map(|s| s.as_bytes().to_vec());
        let path = path.to_string();
        async move { found.ok_or(wado_compiler::SourceError::NotFound { path }) }
    }

    fn emit_diagnostic(&self, diagnostic: wado_compiler::Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

/// Compile `entry` at `-O2` alongside `files`, and return the `remark:`
/// diagnostics as `"file:line:col message"` strings.
fn remarks_across_modules(entry: &str, files: &[(&str, &str)]) -> Vec<String> {
    let host = MultiFileHost {
        files: files
            .iter()
            .map(|(p, s)| ((*p).to_string(), (*s).to_string()))
            .collect(),
        diagnostics: std::sync::Mutex::new(Vec::new()),
    };
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        ..CompilerOptions::default()
    };
    let _ = runtime().block_on(wado_compiler::compile_with_options(
        entry,
        &host,
        Some("test.wado"),
        options,
    ));
    let diagnostics = host.diagnostics.lock().unwrap().clone();
    diagnostics
        .into_iter()
        .filter(|d| d.message.starts_with("remark:"))
        .map(|d| {
            let loc = d
                .span
                .as_ref()
                .map(|s| format!("{}:{}:{}", s.file, s.line, s.column))
                .unwrap_or_default();
            format!("{loc} {}", d.message)
        })
        .collect()
}

#[test]
fn a_copy_in_another_module_of_the_entry_package_is_remarked() {
    // The entry package is more than its entry point. A copy in a local module
    // is the program's own cost, and the remark must name that module's file —
    // a span carries no filename of its own.
    // `seed` keeps the list out of reach of constant folding, which would
    // otherwise evaluate the whole helper away and leave no copy to report.
    let remarks = remarks_across_modules(
        r#"
use { println, Stdout, Environment, args } from "core:cli";
use { grow } from "./helper.wado";

export fn run() with (Stdout, Environment) {
    println(`${grow(args().len() as i32)}`);
}
"#,
        &[(
            "./helper.wado",
            r#"
pub fn grow(seed: i32) -> i32 {
    let a: List<i32> = [seed, seed + 1];
    let mut b = a;
    b.push(seed + 2);
    return a.len() + b.len();
}
"#,
        )],
    );
    assert!(
        remarks
            .iter()
            .any(|r| r.starts_with("./helper.wado:") && r.contains("a copy of")),
        "expected a remark attributed to the local module, got {remarks:?}"
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

/// Compile `source` at `-O2` and return the `remark:` diagnostics naming a
/// region that stayed at run time.
fn const_region_remarks(source: &str) -> Vec<String> {
    remarks_for(source)
        .into_iter()
        .filter(|r| r.contains("computes a constant at run time"))
        .collect()
}

#[test]
fn a_constant_integer_interpolation_folds() {
    // Every interpolation is constant, so the whole template denotes a literal
    // and the buffer, the `Formatter` and `fmt_decimal` all leave with it.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`n=${42}`);
}
"#,
    );

    assert!(remarks.is_empty(), "unexpected remarks: {remarks:?}");
}

#[test]
fn a_constant_string_interpolation_is_not_remarked() {
    // A `String` interpolation folds to a literal today, so nothing survives to
    // report. This is the test that retires itself as coverage grows.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`s=${"y"}`);
}
"#,
    );

    assert!(remarks.is_empty(), "unexpected remarks: {remarks:?}");
}

#[test]
fn a_runtime_interpolation_is_not_remarked() {
    // The region reads a local the optimizer cannot know, so it is not a
    // constant the engine failed to reach — it is a template that has to run.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let n = builtin::black_box(42);
    println(`n=${n}`);
}
"#,
    );

    assert!(remarks.is_empty(), "unexpected remarks: {remarks:?}");
}

#[test]
fn a_materializing_global_store_does_not_refuse_a_region() {
    // `"true"` is globalized, and the store the globalization leaves at the use
    // site sits inside the template region. That store serves the read two
    // statements below it and nothing else, so it is not a write the region is
    // refused for. What the remark reports is the call still standing.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`v=${true}`);
}
"#,
    );

    assert!(
        remarks.is_empty(),
        "the store should not refuse the region: {remarks:?}"
    );
}

#[test]
fn a_template_buffers_inner_block_is_not_remarked() {
    // Every template region contains inner blocks that write the buffer their
    // parent owns. Reporting those would bury the region worth reporting, so a
    // template that does not fold leaves exactly one remark.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`v=${3.5}`);
}
"#,
    );

    assert_eq!(remarks.len(), 1, "expected one remark, got {remarks:?}");
    assert!(
        remarks[0].contains("fmt_into"),
        "the remark should name the float formatter: {remarks:?}"
    );
    assert!(
        !remarks[0].contains("grow"),
        "the buffer's cold reshape is not what the fold waits on: {remarks:?}"
    );
}

#[test]
fn a_list_region_blames_no_call() {
    // A `List<T>` the engine cannot represent stops the fold, and no call on the
    // path is to blame: `push` inlines away and what it leaves is `grow` on a
    // cold path the frame never reaches. Naming that would send the reader after
    // the wrong thing, so the remark says there is nothing to name.
    let remarks = const_region_remarks(
        r#"
use { println, Stdout } from "core:cli";

fn table() -> List<i32> {
    let mut out: List<i32> = List::<i32>::with_capacity(8);
    let mut i = 0;
    while i < 4 {
        out.push(i * 2);
        i += 1;
    }
    return out;
}

export fn run() with Stdout {
    println(`n=${table().len()}`);
}
"#,
    );

    assert!(
        remarks.iter().any(|r| r.contains("no call on its path")),
        "expected the no-call cause, got {remarks:?}"
    );
    assert!(
        remarks.iter().all(|r| !r.contains("grow")),
        "a cold-path call should not be named: {remarks:?}"
    );
}
