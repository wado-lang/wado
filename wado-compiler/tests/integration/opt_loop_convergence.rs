//! The NIR fixed-point loop must converge, not run to its iteration cap. Three
//! ways it failed to: `sroa_variant_return` reporting a change every round for a
//! call site it had already rewritten, `sroa_param` doing the same for a clone
//! it had already minted, and `const_fold` folding a decided-branch chain one
//! link per round.

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

/// A derived `Inspect` separates fields with `if wrote_field { ", " } else
/// { " { "; wrote_field = true }`, so the body is a chain of branches on one
/// flag. Folding it one branch per fixpoint iteration makes the iteration count
/// track the field count, which a 16-field struct alone runs past the cap.
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

fn debug_log(opt_level: OptLevel, opt_iterations: Option<u32>) -> Vec<(Code, String)> {
    debug_log_of(SOURCE, opt_level, opt_iterations)
}

fn iterations_of(log: &[(Code, String)]) -> usize {
    log.iter()
        .filter(|(code, message)| *code == Code::SpanStart && message.starts_with("nir/iteration "))
        .count()
}

fn iterations_run(opt_level: OptLevel) -> usize {
    iterations_of(&debug_log(opt_level, None))
}

fn cap_report(log: &[(Code, String)]) -> Option<&str> {
    log.iter()
        .find(|(_, m)| m.starts_with("NIR optimizer hit the "))
        .map(|(_, m)| m.as_str())
}

#[test]
fn nir_loop_converges_before_the_cap() {
    let o2 = iterations_run(OptLevel::O2);
    assert!(
        o2 < 10,
        "-O2 ran the full iteration cap ({o2}); the loop did not converge"
    );

    let o3 = iterations_run(OptLevel::O3);
    assert!(
        o3 < 20,
        "-O3 ran the full iteration cap ({o3}); the loop did not converge"
    );
}

#[test]
fn converged_run_reports_no_cap() {
    let log = debug_log(OptLevel::O3, None);
    assert_eq!(cap_report(&log), None);
}

/// One iteration is never enough for this source, so the loop exhausts its cap
/// and must say so — naming the passes that were still reporting changes.
#[test]
fn exhausted_cap_is_reported_with_the_passes_still_changing() {
    let log = debug_log(OptLevel::O3, Some(1));
    let report = cap_report(&log).expect("the exhausted cap should be reported");
    assert!(
        report.starts_with(
            "NIR optimizer hit the 1-iteration cap without converging; still changing: [nir/"
        ),
        "unexpected report: {report}"
    );
}

/// A decided branch runs exactly one arm, so folding must carry the env through
/// it. Dropping what the arm writes instead costs one iteration per link of the
/// chain, which shows up as an iteration count that scales with the field count.
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
