//! `wado dump --inline-decisions` reports what the inliner decided at each
//! site and why, for the functions the pattern names, the same whatever the
//! thread count.

use crate::common::{block_on, in_memory_host};
use wado_compiler::{OptLevel, dump_with_host_and_world};

const SOURCE: &str = r#"
fn medium(x: i32) -> i32 { return x * 3 + x / 7 + (x ^ 5) * 11; }

fn work(n: i32) -> i32 {
    let mut t = medium(n);
    if n < 0 {
        builtin::cold_path();
        t += medium(builtin::black_box(n));
    }
    return t;
}

export fn run() {
    builtin::black_box(work(builtin::black_box(10)));
}
"#;

fn decisions(pattern: &str, parallelism: usize) -> String {
    dump(Some(pattern), parallelism)
        .inline_decisions_text
        .expect("decisions recorded")
}

fn dump(pattern: Option<&str>, parallelism: usize) -> wado_compiler::DumpResult {
    let host = in_memory_host(&[]);
    block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        wado_compiler::OptOverrides::default(),
        &[],
        None,
        &wado_compiler::param_resolution::ParamInputs::default(),
        wado_compiler::kiln::InvocationIndex::default(),
        parallelism,
        pattern,
    ))
    .expect("dump succeeds")
}

#[test]
fn a_site_names_its_decision_and_why() {
    let log = decisions("medium", 1);
    assert!(
        log.contains(
            "callee `medium`: hot 6, size 6, 1 params, 2 sites -> everywhere: under the threshold"
        ),
        "{log}"
    );
    assert!(
        log.contains("site `work` entry.wado:5:17 (plain) -> `medium`: spliced"),
        "{log}"
    );
    assert!(
        log.contains(
            "site `work` entry.wado:8:14 (cold, in the run from entry.wado:7:9) -> `medium`: \
             declined, a cold site takes only `#[inline(always)]` or a callee that shrinks it"
        ),
        "{log}"
    );
}

#[test]
fn the_pattern_keeps_only_the_functions_it_names() {
    let log = decisions("work", 1);
    assert!(!log.contains("callee `medium`"), "{log}");
    assert!(log.contains("site `work`"), "{log}");
    assert!(log.contains("-> `work`"), "{log}");
}

#[test]
fn the_log_is_the_same_on_any_thread_count() {
    assert_eq!(decisions("", 1), decisions("", 4));
}

#[test]
fn recording_changes_nothing_it_records() {
    let wir = |pattern| {
        wado_compiler::wir_unparse::unparse_wir(&dump(pattern, 1).wir_package.expect("WIR built"))
    };
    assert_eq!(wir(Some("")), wir(None));
}
