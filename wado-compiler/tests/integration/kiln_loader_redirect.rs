//! Test that the loader's Kiln invocation redirect correctly rewrites a
//! `use { X } from "<schema>"` clause to the generator's emitted entry
//! module.

use crate::common::{MapHost, block_on};
use wado_compiler::{
    CompilerHost, LogLevel, Semantics, kiln::InvocationIndex, load, parse, semantics_of,
};

/// Run the three-stage frontend (parse → load → `semantics_of`) with the
/// given kiln invocation index. Test-local helper that mirrors the
/// shape of `Engine::snapshot`'s build path.
fn build_with_invocations(
    source: &str,
    filename: &str,
    host: &impl CompilerHost,
    invocations: InvocationIndex,
) -> Semantics {
    block_on(async {
        let parsed = parse(source);
        assert!(
            parsed.lex_errors.is_empty() && parsed.errors.is_empty(),
            "entry source should parse cleanly: lex={:?} parse={:?}",
            parsed.lex_errors,
            parsed.errors,
        );
        let loaded = load(
            parsed,
            Some(filename),
            host,
            invocations,
            LogLevel::default(),
        )
        .await
        .expect("loader should succeed in this fixture");
        semantics_of(loaded, host, LogLevel::default(), true)
    })
}

#[test]
fn invocation_index_redirects_use_from_schema_to_generated_entry() {
    let entry = r#"
use { greet } from "./sample.proto";

export fn run() {
    greet();
}
"#;
    let generated = r"
pub fn greet() {}
";
    let host = MapHost::new(&[("build/kiln/test-invocation/sample.wado", generated)]);

    let mut idx = InvocationIndex::new();
    idx.insert(
        "entry.wado",
        "./sample.proto",
        "build/kiln/test-invocation/sample.wado",
    );

    let sem = build_with_invocations(entry, "entry.wado", &host, idx);
    if !sem.is_complete() {
        let diags = host.diagnostics();
        panic!(
            "semantics did not complete; diagnostics: {:#?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    let entry_ms = sem.interner.borrow_mut().entry_point("entry.wado");
    let redirected = sem
        .interner
        .borrow_mut()
        .redirected("build/kiln/test-invocation/sample.wado", &entry_ms);
    assert!(
        sem.modules.contains_key(&redirected),
        "loader should have loaded the generated entry module, got: {:?}",
        sem.modules.keys().collect::<Vec<_>>()
    );
}

#[test]
fn generated_module_imports_its_sibling_relative_to_itself() {
    let entry = r#"
use { greet } from "./sample.proto";

export fn run() {
    greet();
}
"#;
    let generated = r#"
use { helper } from "./helper.wado";
pub fn greet() { helper(); }
"#;
    let helper = r"
pub fn helper() {}
";
    let host = MapHost::new(&[
        ("build/kiln/test-invocation/sample.wado", generated),
        ("build/kiln/test-invocation/helper.wado", helper),
    ]);

    let mut idx = InvocationIndex::new();
    idx.insert(
        "entry.wado",
        "./sample.proto",
        "build/kiln/test-invocation/sample.wado",
    );

    let sem = build_with_invocations(entry, "entry.wado", &host, idx);
    let diags = host.diagnostics();
    assert!(
        sem.is_complete(),
        "semantics did not complete; diagnostics: {:#?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let entry_ms = sem.interner.borrow_mut().entry_point("entry.wado");
    let sibling = sem
        .interner
        .borrow_mut()
        .redirected("build/kiln/test-invocation/helper.wado", &entry_ms);
    assert!(
        sem.modules.contains_key(&sibling),
        "the sibling should load as a generated module, got: {:?}",
        sem.modules.keys().collect::<Vec<_>>()
    );
}

#[test]
fn empty_invocation_index_preserves_default_resolution() {
    let entry = r"
export fn run() {}
";
    let host = MapHost::new(&[]);
    let idx = InvocationIndex::new();
    let sem = build_with_invocations(entry, "entry.wado", &host, idx);
    let entry_ms = sem.interner.borrow_mut().entry_point("entry.wado");
    assert!(sem.modules.contains_key(&entry_ms));
}
