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
    for level in [OptLevel::O2, OptLevel::O3] {
        let log = debug_log_of(SOURCE, level, None);
        assert_eq!(cap_report(&log), None, "{level:?} did not converge");
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

/// A decided branch runs exactly one arm, so folding must carry the env through
/// it — otherwise the iteration count scales with the chain's length.
#[test]
fn inspect_chain_folds_in_one_iteration_per_struct_not_per_field() {
    let four_log = debug_log_of(&inspect_chain_source(4), OptLevel::O3, None);
    let sixteen_log = debug_log_of(&inspect_chain_source(16), OptLevel::O3, None);
    // Without this, both runs reaching the cap would compare equal and pass.
    assert_eq!(cap_report(&four_log), None);
    assert_eq!(cap_report(&sixteen_log), None);
    let (four, sixteen) = (iterations_of(&four_log), iterations_of(&sixteen_log));
    assert!(
        sixteen <= four,
        "iteration count tracks the field count ({four} for 4 fields, {sixteen} for 16)"
    );
}
