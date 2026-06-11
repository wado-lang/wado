//! R1 validation for the WIR-level component interface plan (WEP
//! `wep-2026-05-02-wit-interoperability.md` §"Faithful imports").
//!
//! For each program we compute the plan
//! (`NirPackage::imported_cm_interfaces`, via `dump`) and the actual CM
//! interface imports of the compiled component (via `wasmprinter`), and assert
//! they are equal. This proves the plan faithfully mirrors what codegen emits.

#![allow(unused_crate_dependencies)]

mod common;

use std::collections::BTreeSet;

use common::InMemoryHost;
use wado_compiler::{
    CompilerOptions, OptLevel, compile_with_host, compile_with_options, dump_with_host_and_world,
};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// The plan: `NirPackage::imported_cm_interfaces`, obtained from a dump.
fn plan_imports(source: &str) -> BTreeSet<String> {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        source,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        None,
        &[],
    ))
    .expect("dump succeeds");
    let pkg = dump
        .optimized_package
        .expect("optimized package present after dump");
    pkg.imported_cm_interfaces.iter().cloned().collect()
}

/// The ground truth: CM interface FQs the compiled component actually imports,
/// scraped from the printed WAT.
fn actual_imports(source: &str) -> BTreeSet<String> {
    let host = InMemoryHost::new();
    let result = block_on(compile_with_host(
        source,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
    ))
    .expect("compile succeeds");
    let mut wat = String::new();
    wasmprinter::Config::new()
        .print(&result.wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .expect("print wat");

    let mut imports = BTreeSet::new();
    for fragment in wat.split("(import \"").skip(1) {
        let Some(name) = fragment.split('"').next() else {
            continue;
        };
        // CM interface FQs look like `wasi:cli/stdout@<version>`; canonical
        // intrinsics (`wasi`, `mem`, ...) and core imports do not.
        if (name.starts_with("wasi:") || name.starts_with("core:"))
            && name.contains('/')
            && name.contains('@')
        {
            imports.insert(name.to_string());
        }
    }
    imports
}

fn read_corpus(rel: &str) -> String {
    let path = format!("{}/../{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Assert the plan equals the compiled component's CM imports for `rel`.
fn assert_corpus_plan_matches(rel: &str) -> BTreeSet<String> {
    let source = read_corpus(rel);
    let plan = plan_imports(&source);
    let actual = actual_imports(&source);
    assert_eq!(
        plan, actual,
        "import plan diverges from the compiled component for {rel}\nplan:   {plan:?}\nactual: {actual:?}"
    );
    plan
}

#[test]
fn plan_matches_component_across_cli_corpus() {
    for rel in [
        "example/fizzbuzz.wado",
        "example/hello.wado",
        "example/sha256.wado",
        "example/uuid_v7.wado",
        "benchmark/count_prime/count_prime.wado",
        "benchmark/mandelbrot/mandelbrot.wado",
        "benchmark/sieve/sieve.wado",
    ] {
        assert_corpus_plan_matches(rel);
    }
}

#[test]
fn plan_matches_component_for_pure_compute() {
    // A program that references no error-code drops the dead wasi:cli/types
    // import; the plan and the component agree (both empty here).
    let source = "export fn run() {\n\
                      let mut x = 0;\n\
                      let mut i = 0;\n\
                      while i < 100 { x += i; i += 1; }\n\
                  }";
    let plan = plan_imports(source);
    let actual = actual_imports(source);
    assert_eq!(plan, actual, "\nplan:   {plan:?}\nactual: {actual:?}");
    assert!(!plan.iter().any(|i| i.contains("cli/types")), "{plan:?}");
}

#[test]
fn plan_includes_implicit_stderr_when_component_does() {
    // sha256 uses asserts on runtime values, so the real component imports
    // wasi:cli/stderr; the plan must include it (it is not in any `with` row).
    let imports = assert_corpus_plan_matches("example/sha256.wado");
    assert!(
        imports.iter().any(|i| i.contains("cli/stderr")),
        "{imports:?}"
    );
}

#[test]
fn plan_matches_component_for_http_service_with_resources() {
    // The HTTP handler exercises the resource-closure and HTTP phases of the
    // plan builder (request/response/fields resources, wasi:http/types).
    let source = read_corpus("example/http_server.wado");
    let world = "wasi:http/service";

    let plan: BTreeSet<String> = {
        let host = InMemoryHost::new();
        let dump = block_on(dump_with_host_and_world(
            &source,
            &host,
            Some("entry.wado"),
            OptLevel::O2,
            Some(world),
            None,
            None,
            &[],
        ))
        .expect("dump succeeds");
        dump.optimized_package
            .expect("optimized package present")
            .imported_cm_interfaces
            .iter()
            .cloned()
            .collect()
    };

    let actual: BTreeSet<String> = {
        let host = InMemoryHost::new();
        let options = CompilerOptions {
            target_world: Some(world.to_string()),
            ..Default::default()
        };
        let result = block_on(compile_with_options(
            &source,
            &host,
            Some("entry.wado"),
            options,
        ))
        .expect("compile succeeds");
        let mut wat = String::new();
        wasmprinter::Config::new()
            .print(&result.wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
            .expect("print wat");
        wat.split("(import \"")
            .skip(1)
            .filter_map(|frag| frag.split('"').next())
            .filter(|n| {
                (n.starts_with("wasi:") || n.starts_with("core:"))
                    && n.contains('/')
                    && n.contains('@')
            })
            .map(str::to_string)
            .collect()
    };

    assert_eq!(
        plan, actual,
        "http import plan diverges\nplan:   {plan:?}\nactual: {actual:?}"
    );
    assert!(plan.iter().any(|i| i.contains("http/types")), "{plan:?}");
}

#[test]
fn plan_excludes_type_alias_only_clock_types() {
    // count_prime uses MonotonicClock; the real component imports
    // monotonic-clock but not clocks/types (which only provides `duration`).
    let imports = assert_corpus_plan_matches("benchmark/count_prime/count_prime.wado");
    assert!(
        imports.iter().any(|i| i.contains("clocks/monotonic-clock")),
        "{imports:?}"
    );
    assert!(
        !imports.iter().any(|i| i.contains("clocks/types")),
        "{imports:?}"
    );
}
