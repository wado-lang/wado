//! The NIR fixed-point loop must converge, not run to its iteration cap, and
//! must say which passes held it open when it does not.

use std::fmt::Write as _;

use wado_compiler::{Code, CompilerOptions, LogLevel, OptLevel, Severity};

const SOURCE: &str = r#"
#[inline("never")]
fn lookup(xs: &List<i32>, key: i32) -> Result<i32, String> {
    let mut i = 0;
    while i < xs.len() {
        if xs[i] == key {
            return Result::<i32, String>::Ok(i as i32);
        }
        i = i + 1;
    }
    return Result::<i32, String>::Err(`missing ${key}`);
}

#[inline("never")]
fn describe(xs: &List<i32>, key: i32) -> String {
    let found = lookup(xs, key);
    match found {
        Ok(i) => { return `at ${i}`; }
        Err(e) => { return e; }
    }
}

export fn run() {
    let xs: List<i32> = [3, 1, 4, 1, 5];
    assert describe(&xs, 4) == "at 2";
    assert describe(&xs, 9) == "missing 9";
}
"#;

/// Folding leaves the parse a call in drop position, whose scalarized
/// `Result` nothing reads.
const DISCARDED_RESULT_SOURCE: &str = r#"
use { println, Stdout } from "core:cli";

fn parse_scalar<T: FromStr>(s: &String) -> Result<T, i32> {
    return match T::from_str_range(s, 0, 3) {
        Ok(v) => Result::Ok(v),
        Err(_) => Result::Err(-1),
    };
}

export fn run() with Stdout {
    let r = parse_scalar::<u64>(&"123");
    if let Ok(v) = r {
        println(`${v}`);
    } else {
        println("err");
    }
}
"#;

/// A struct whose derived `Inspect` is a chain of `if wrote_field { ", " } else
/// { " { "; wrote_field = true }`, one link per field.
fn inspect_chain_source(fields: usize) -> String {
    let mut decls = String::new();
    let mut inits = String::new();
    for i in 0..fields {
        writeln!(decls, "    f{i}: i32,").unwrap();
        writeln!(inits, "        f{i}: builtin::black_box({i}),").unwrap();
    }
    format!(
        "use {{ print, Stdout }} from \"core:cli\";\n\
         struct Node {{\n{decls}}}\n\
         export fn run() with (Stdout) {{\n\
         \x20   let n = Node {{\n{inits}    }};\n\
         \x20   print(`${{n:?}}`)\n\
         }}\n"
    )
}

/// `depth` functions forwarding a parameter no one reads — the shape a
/// Gale-generated parser's `follow` argument has.
fn forwarding_chain_source(depth: usize) -> String {
    let mut fns = String::new();
    fns.push_str(
        "#[inline(\"never\")]\nfn f0(x: i32, follow: i32) -> i32 {\n    return x + 1;\n}\n",
    );
    for i in 1..depth {
        writeln!(
            fns,
            "#[inline(\"never\")]\nfn f{i}(x: i32, follow: i32) -> i32 {{\n    \
             return f{}(x, follow);\n}}",
            i - 1
        )
        .unwrap();
    }
    format!(
        "{fns}\nexport fn run() {{\n\
         \x20   assert f{}(builtin::black_box(1), builtin::black_box(2)) == 2;\n\
         }}\n",
        depth - 1
    )
}

fn debug_log_of(
    source: &str,
    opt_level: OptLevel,
    opt_iterations: Option<u32>,
) -> Vec<(Code, String)> {
    let host = crate::common::InMemoryHost::new();
    let options = CompilerOptions {
        opt_level,
        opt_iterations,
        log_level: Some(LogLevel::Debug),
        ..Default::default()
    };
    crate::common::runtime()
        .block_on(wado_compiler::compile_with_options(
            source,
            &host,
            Some("opt_loop_convergence.wado"),
            options,
        ))
        .expect("compilation should succeed");

    host.diagnostics()
        .iter()
        .filter(|d| d.severity == Severity::Debug)
        .map(|d| (d.code, d.message.clone()))
        .collect()
}

fn iterations_of(log: &[(Code, String)]) -> usize {
    log.iter()
        .filter(|(code, message)| *code == Code::SpanStart && message.starts_with("nir/iteration "))
        .count()
}

fn cap_report(log: &[(Code, String)]) -> Option<&str> {
    log.iter()
        .find(|(_, m)| m.starts_with("NIR optimizer hit the "))
        .map(|(_, m)| m.as_str())
}

#[test]
fn nir_loop_converges_before_the_cap() {
    for source in [SOURCE, DISCARDED_RESULT_SOURCE] {
        for level in [OptLevel::O2, OptLevel::O3] {
            let log = debug_log_of(source, level, None);
            assert_eq!(cap_report(&log), None, "{level:?} did not converge");
        }
    }
}

/// One iteration is never enough for this source, so the loop exhausts its cap
/// and must say so — naming the passes that were still reporting changes.
#[test]
fn exhausted_cap_is_reported_with_the_passes_still_changing() {
    let log = debug_log_of(SOURCE, OptLevel::O3, Some(1));
    let report = cap_report(&log).expect("the exhausted cap should be reported");
    assert!(
        report.starts_with(
            "NIR optimizer hit the 1-iteration cap without converging; still changing: [nir/"
        ),
        "unexpected report: {report}"
    );
}

/// Lengthening a chain must not lengthen the loop: a pass that takes one link
/// of it per iteration is what puts the count on the program's size.
fn assert_iterations_hold(
    source: impl Fn(usize) -> String,
    level: OptLevel,
    iterations: Option<u32>,
    (small, large): (usize, usize),
) {
    let small_log = debug_log_of(&source(small), level, iterations);
    let large_log = debug_log_of(&source(large), level, iterations);
    // Both runs reaching the cap would compare equal and pass.
    assert_eq!(cap_report(&small_log), None);
    assert_eq!(cap_report(&large_log), None);
    let (a, b) = (iterations_of(&small_log), iterations_of(&large_log));
    assert!(b <= a, "{a} iterations at {small}, {b} at {large}");
}

/// A decided branch runs exactly one arm, so folding must carry the env through
/// it rather than dropping what that arm writes.
#[test]
fn inspect_chain_folds_per_struct_not_per_field() {
    assert_iterations_hold(inspect_chain_source, OptLevel::O3, None, (4, 16));
}

/// Dropping a dead parameter kills the argument its callers pass, so `dae` has a
/// fixed point of its own to reach.
#[test]
fn forwarding_chain_folds_per_program_not_per_link() {
    // Explicit iterations: a regression would otherwise reach the level's cap
    // and trip the convergence assert before this test could read the count.
    assert_iterations_hold(forwarding_chain_source, OptLevel::O2, Some(60), (5, 20));
}
