//! Integration tests for `Engine::diagnostics`.
//!
//! Scope: what the *engine* does to a diagnostic, not which diagnostic the
//! compiler raises. A test belongs here only if it would still fail with a
//! correct compiler — per-document filtering, LSP tags, the live
//! `set_unused_diagnostics` toggle, the checks the no-TIR path must still run,
//! and opening a bundled stdlib module as the entry document.
//!
//! "Source X raises diagnostic Y" is a compiler question and belongs in
//! `wado-compiler/tests/fixtures/` (`compile_error` / `compile_errors_contains`
//! / `warnings_contains` / `warnings_not_contains`), where the whole corpus
//! enforces it.

use wado_lsp::test_support::MapHost;
use wado_lsp::{Diagnostic, DiagnosticTag, Engine, Severity};

async fn diagnostics_for(path: &str, source: &str) -> Vec<Diagnostic> {
    let uri = format!("file://{path}");
    let host = MapHost::empty();
    let mut engine = Engine::new();
    engine.open_document(&uri, source.to_string());
    engine.diagnostics(&uri, &host).await
}

fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

/// Dead-code diagnostics carry the LSP `Unnecessary` tag.
#[test]
fn dead_code_diagnostics_carry_unnecessary_tag() {
    futures::executor::block_on(async {
        let source = "fn unused_helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {}\n";
        let diags = diagnostics_for("/work/tag.wado", source).await;
        let dead = diags
            .iter()
            .find(|d| d.message.contains("function `unused_helper` is never used"))
            .expect("expected a dead-function warning");
        assert_eq!(
            dead.tags,
            vec![DiagnosticTag::Unnecessary],
            "dead-code warning must carry the Unnecessary tag, got {dead:#?}"
        );
    });
}

/// Dead code in an imported module stays off the importer's diagnostics but
/// still appears when that module is opened directly.
#[test]
fn imported_module_dead_code_stays_off_the_importer() {
    futures::executor::block_on(async {
        let bar = "pub fn helper() -> i32 {\n    return 1;\n}\n\ninternal fn dead_in_bar() -> i32 {\n    return 2;\n}\n";
        let foo =
            "use { helper } from \"./bar.wado\";\n\nexport fn run() {\n    let _ = helper();\n}\n";
        let host = MapHost::with_files(&[("/work/bar.wado", bar)]);
        let mut engine = Engine::new();
        engine.open_document("file:///work/foo.wado", foo.to_string());
        engine.open_document("file:///work/bar.wado", bar.to_string());

        let foo_diags = engine.diagnostics("file:///work/foo.wado", &host).await;
        assert!(
            !foo_diags.iter().any(|d| d.message.contains("dead_in_bar")),
            "imported module's dead code must not appear on the importer, got {foo_diags:#?}"
        );

        let bar_diags = engine.diagnostics("file:///work/bar.wado", &host).await;
        assert!(
            bar_diags
                .iter()
                .any(|d| d.message.contains("function `dead_in_bar` is never used")),
            "bar.wado's own diagnostics must report its dead code, got {bar_diags:#?}"
        );
    });
}

/// An *error* in an imported module follows the same rule as its dead code:
/// it belongs to that module's document, not the importer's. Published on the
/// importer it lands at the imported file's line and column, over unrelated
/// code.
#[test]
fn imported_module_errors_stay_off_the_importer() {
    futures::executor::block_on(async {
        let bar = "pub fn boom() -> i32 {\n    return \"not an i32\";\n}\n";
        let foo =
            "use { boom } from \"./bar.wado\";\n\nexport fn run() {\n    let _ = boom();\n}\n";
        let host = MapHost::with_files(&[("/work/bar.wado", bar)]);
        let mut engine = Engine::new();
        engine.open_document("file:///work/foo.wado", foo.to_string());
        engine.open_document("file:///work/bar.wado", bar.to_string());

        let foo_diags = engine.diagnostics("file:///work/foo.wado", &host).await;
        assert!(
            !foo_diags
                .iter()
                .any(|d| d.message.contains("type mismatch")),
            "bar.wado's type error must not be published on foo.wado, got {foo_diags:#?}"
        );

        let bar_diags = engine.diagnostics("file:///work/bar.wado", &host).await;
        assert!(
            errors(&bar_diags)
                .iter()
                .any(|d| d.message.contains("type mismatch")),
            "bar.wado's own diagnostics must report its type error, got {bar_diags:#?}"
        );
    });
}

/// A broken import must surface as an error on the importing document.
///
/// `ModuleNotFound` carries no span, and the same failure empties the
/// `Semantics` — without the diagnostic the file looks clean and answers
/// nothing.
#[test]
fn missing_import_is_reported_on_the_importer() {
    futures::executor::block_on(async {
        let source = "use { nope } from \"./missing.wado\";\n\nexport fn run() {}\n";
        let diags = diagnostics_for("/work/broken.wado", source).await;
        let errs = errors(&diags);
        assert!(
            errs.iter().any(|d| d.message.contains("./missing.wado")),
            "a broken import must be reported, got {diags:#?}"
        );
        assert!(
            errs.iter().all(|d| d.range.start.line == 0),
            "a span-less loader failure anchors at the document start, got {errs:#?}"
        );
    });
}

/// `set_unused_diagnostics(false)` takes effect at the next query, no edit.
#[test]
fn toggling_unused_diagnostics_takes_effect_without_reopen() {
    futures::executor::block_on(async {
        let source = "fn unused_helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {}\n";
        let host = MapHost::empty();
        let mut engine = Engine::new();
        engine.open_document("file:///work/toggle.wado", source.to_string());

        let before = engine.diagnostics("file:///work/toggle.wado", &host).await;
        assert!(
            before.iter().any(|d| d.message.contains("unused_helper")),
            "dead code should be reported while enabled, got {before:#?}"
        );

        engine.set_unused_diagnostics(false);
        let after = engine.diagnostics("file:///work/toggle.wado", &host).await;
        assert!(
            !after.iter().any(|d| d.message.contains("unused_helper")),
            "disabling must suppress dead code at the next query, got {after:#?}"
        );
    });
}

/// A bundled module opened as the entry document is clean. Each carries
/// `#![no_prelude]` (gating the prelude-collision check against the types it
/// itself supplies) and `#![stdlib(...)]`, which pins the entry's
/// `ModuleSource` to its bundled identity — without it the entry copy and the
/// cached copy both define `Option` / `Waitable` / … and collide.
///
/// One row per shape the identity resolution distinguishes: a leaf module, a
/// re-export root, a wasi sub-interface, and a wasi flat-package root
/// (`#![generated]`, no sub-interface).
#[test]
fn opening_a_bundled_module_is_clean() {
    let cases: &[(&str, &str)] = &[
        (
            "/work/wado-compiler/lib/core/prelude/types.wado",
            include_str!("../../wado-compiler/lib/core/prelude/types.wado"),
        ),
        (
            "/work/wado-compiler/lib/core/prelude.wado",
            include_str!("../../wado-compiler/lib/core/prelude.wado"),
        ),
        (
            "/work/wado-compiler/lib/wasi/cli/stdout.wado",
            include_str!("../../wado-compiler/lib/wasi/cli/stdout.wado"),
        ),
        (
            "/work/wado-compiler/lib/wasi/cli.wado",
            include_str!("../../wado-compiler/lib/wasi/cli.wado"),
        ),
    ];
    futures::executor::block_on(async {
        for (path, source) in cases {
            let diags = diagnostics_for(path, source).await;
            let errs = errors(&diags);
            assert!(
                errs.is_empty(),
                "opening {path} must be clean, got {}: {errs:#?}",
                errs.len(),
            );
        }
    });
}

/// The semantic checks are derived from `Semantics`, not from TIR, so they are
/// the ones the LSP path can still run — and it must run every kind
/// `Snapshot::semantic_diagnostics` assembles, not just the first. One row per
/// kind; whether each check is *right* is the compiler corpus's question.
#[test]
fn every_semantic_check_reaches_the_editor() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "effects",
            r#"
use { println, Stdout } from "core:cli";

fn greet() with Stdout {
    println("hi");
}

fn bad() {
    greet();
}

export fn run() with Stdout {
    println("ok");
}
"#,
            "missing effect",
        ),
        (
            "stores",
            r#"
struct Data {
    value: i32,
}

fn bad_return(data: &Data) -> &Data {
    return data;
}

export fn run() {}
"#,
            "stores[data]",
        ),
        (
            "purity",
            r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("x");
    return 1;
}

fn greet(value: i32 = noisy()) -> i32 {
    return value;
}

export fn run() {}
"#,
            "must be pure",
        ),
        (
            "resource-moves",
            r#"
use { Fields } from "wasi:http";

struct Holder {
    f: Fields,
}

impl Holder {
    fn peek(&self) -> Fields {
        return self.f;
    }
}

export fn run() {}
"#,
            "cannot move resource `Fields` out of a borrow",
        ),
    ];
    futures::executor::block_on(async {
        for (kind, source, expected) in cases {
            let diags = diagnostics_for(&format!("/work/{kind}.wado"), source).await;
            assert!(
                diags.iter().any(|d| d.message.contains(expected)),
                "the {kind} check must reach the editor: expected {expected:?}, got {diags:#?}"
            );
        }
    });
}
