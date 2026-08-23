//! `#[cm(..., type="extern-ref")]` marks a resource as a host-object handle:
//! copyable, and outside the affine discipline every other resource follows.
//! See `docs/wep-2026-04-28-resource-inheritance.md`.

use crate::common::InMemoryHost;
use wado_compiler::check_resource_moves_semantic;
use wado_compiler::semantics::semantics;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn move_errors(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_resource_moves_semantic(&sem)
        .into_iter()
        .map(|e| e.to_string())
        .collect()
}

const USE_TWICE: &str = r#"
fn consume(h: Handle) {}

fn use_twice(h: Handle) {
    consume(h);
    consume(h);
}

export fn run() {}
"#;

#[test]
fn a_plain_resource_is_move_only() {
    let source = format!("resource Handle {{}}\n{USE_TWICE}");
    let errors = move_errors(&source);
    assert!(
        errors.iter().any(|e| e.contains("use_twice") || e.contains('h')),
        "expected a use-after-move error, got {errors:?}"
    );
}

#[test]
fn an_extern_ref_resource_is_copyable() {
    let source = format!(
        "#[cm(\"web:dom/handle\", type=\"extern-ref\")]\nresource Handle {{}}\n{USE_TWICE}"
    );
    let errors = move_errors(&source);
    assert!(
        errors.is_empty(),
        "an extern-ref handle is copyable, got {errors:?}"
    );
}

#[test]
fn an_i32_backed_resource_stays_move_only() {
    let source =
        format!("#[cm(\"wasi:demo/handle\", type=\"i32\")]\nresource Handle {{}}\n{USE_TWICE}");
    let errors = move_errors(&source);
    assert!(
        !errors.is_empty(),
        "an i32-backed handle keeps the affine discipline"
    );
}
