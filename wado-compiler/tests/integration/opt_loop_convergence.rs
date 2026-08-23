//! The NIR fixed-point loop must converge, not run to its iteration cap.
//!
//! `sroa_variant_return` reported a change on every round for a call site it
//! had already rewritten, so the loop ran the full cap (10 rounds at `-O2`,
//! 30 at `-O3`) on any program with a scalarized variant return — and every
//! other gated pass re-scanned the functions it re-dirtied.

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

/// How many `nir/iteration N` spans the optimizer opened.
fn iterations_run(opt_level: OptLevel) -> usize {
    let host = crate::common::InMemoryHost::new();
    let options = CompilerOptions {
        opt_level,
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
        .filter(|d| {
            d.severity == Severity::Debug
                && d.code == Code::SpanStart
                && d.message.starts_with("nir/iteration ")
        })
        .count()
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
        o3 < 30,
        "-O3 ran the full iteration cap ({o3}); the loop did not converge"
    );
}
