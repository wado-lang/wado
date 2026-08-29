//! The NIR fixed-point loop must converge, not run to its iteration cap.
//! `sroa_variant_return` reported a change every round for a call site it had
//! already rewritten, which held the loop open to the cap.

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

fn debug_log(opt_level: OptLevel, opt_iterations: Option<u32>) -> Vec<(Code, String)> {
    let host = crate::common::InMemoryHost::new();
    let options = CompilerOptions {
        opt_level,
        opt_iterations,
        log_level: Some(LogLevel::Debug),
        ..Default::default()
    };
    crate::common::runtime()
        .block_on(wado_compiler::compile_with_options(
            SOURCE,
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

fn iterations_run(opt_level: OptLevel) -> usize {
    debug_log(opt_level, None)
        .iter()
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
