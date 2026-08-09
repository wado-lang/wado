//! `wado dump` must lower with the same last-use move analysis as
//! `compile`. Regression: the dump pipeline never set
//! `Package::moved_local_spans`, so every last use lowered as a
//! defensive `$value_copy$` the real compilation never emits.

use crate::common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

const SOURCE: &str = r#"
fn consume(xs: List<i32>) -> i32 {
    return xs.len();
}

export fn run() {
    let a: List<i32> = [1, 2, 3];
    let n = consume(a);
    assert n == 3;
}
"#;

#[test]
fn dump_applies_last_use_moves() {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O0,
        None,
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dump succeeds");

    let lowered = dump
        .lowered_nir_text
        .expect("lowered NIR text present after dump");

    let run_body = lowered
        .split("export fn run()")
        .nth(1)
        .and_then(|rest| rest.split("\nfn ").next())
        .expect("entry `run` present in lowered NIR");

    assert!(
        run_body.contains("consume"),
        "run body should call consume, got:\n{run_body}"
    );
    assert!(
        !run_body.contains("$value_copy$"),
        "`consume(a)` is `a`'s last use and must lower as a move, \
         not a `$value_copy$` wrap:\n{run_body}"
    );
}
