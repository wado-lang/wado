//! Integration tests for `Engine::diagnostics`.
//!
//! `Engine::diagnostics` runs only the semantics pipeline (parse → bind →
//! load → analyze → resolve), deliberately stopping before
//! codegen. These tests pin two contracts:
//!
//! - Bundled stdlib files carry `#![no_prelude]`, so opening them in an
//!   editor produces a clean compile (no `PreludeTypeCollision` against
//!   the types they themselves define for the prelude).
//! - User code that redefines a prelude name without opting out via
//!   `#![no_prelude]` still surfaces the collision diagnostic.
//! - Inputs that historically panicked during codegen validation now
//!   complete cleanly because `Engine::diagnostics` stops at
//!   `semantics_of`.

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

fn warnings(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .collect()
}

/// A private function with no caller surfaces as a dead-code warning in the
/// editor. The compiler already computes source-level liveness on the LSP
/// path (`build_tir = false`); `Engine::diagnostics` now reads it.
#[test]
fn dead_function_is_reported() {
    futures::executor::block_on(async {
        let source = "fn unused_helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {}\n";
        let diags = diagnostics_for("/work/dead_fn.wado", source).await;
        assert!(
            warnings(&diags)
                .iter()
                .any(|d| d.message.contains("function `unused_helper` is never used")),
            "expected a dead-function warning, got {diags:#?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("function `run` is never used")),
            "the world-export root `run` must not be reported, got {diags:#?}"
        );
    });
}

/// A global with no reader surfaces as a dead-code warning.
#[test]
fn dead_global_is_reported() {
    futures::executor::block_on(async {
        let source = "global UNUSED: i32 = 42;\n\nexport fn run() {}\n";
        let diags = diagnostics_for("/work/dead_global.wado", source).await;
        assert!(
            warnings(&diags)
                .iter()
                .any(|d| d.message.contains("global `UNUSED` is never used")),
            "expected a dead-global warning, got {diags:#?}"
        );
    });
}

/// A `pub fn` crosses the package boundary and is a liveness root, so it is
/// never reported even without an in-package caller.
#[test]
fn pub_function_is_not_reported() {
    futures::executor::block_on(async {
        let source = "pub fn library_api() -> i32 {\n    return 1;\n}\n\nexport fn run() {}\n";
        let diags = diagnostics_for("/work/pub_fn.wado", source).await;
        assert!(
            !diags.iter().any(|d| d.message.contains("library_api")),
            "a `pub fn` must not be reported as dead, got {diags:#?}"
        );
    });
}

/// A function reached only from a `test` block is reported test-only (the
/// editor runs in the default command world, so `is_test_world` is false).
#[test]
fn test_only_function_is_reported() {
    futures::executor::block_on(async {
        let source = "fn helper() -> i32 {\n    return 1;\n}\n\nexport fn run() {}\n\ntest \"uses helper\" {\n    assert helper() == 1;\n}\n";
        let diags = diagnostics_for("/work/test_only.wado", source).await;
        assert!(
            warnings(&diags).iter().any(|d| d
                .message
                .contains("function `helper` is only used by tests")),
            "expected a test-only warning, got {diags:#?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("function `helper` is never used")),
            "a test-reached function must not be reported dead, got {diags:#?}"
        );
    });
}

/// Dead-code diagnostics carry the LSP `Unnecessary` tag so the editor fades
/// their range; ordinary errors do not.
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

/// A clean program whose every item is reached emits no dead-code warnings —
/// in particular the imported stdlib items are never reported.
#[test]
fn fully_used_program_has_no_dead_warnings() {
    futures::executor::block_on(async {
        let source = "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"ok\");\n}\n";
        let diags = diagnostics_for("/work/clean.wado", source).await;
        assert!(
            warnings(&diags).is_empty(),
            "expected no dead-code warnings for a fully-used program, got {diags:#?}"
        );
    });
}

/// Opening `core:prelude/types.wado` is clean: the file declares both
/// `#![no_prelude]` (gating the prelude-collision check) and
/// `#![stdlib("core:prelude/types.wado")]` (pinning the entry's
/// `ModuleSource` to its bundled identity, which dedups the entry against
/// the cache and so eliminates the cross-module duplicate-definition
/// errors on `Option` / `Waitable` / … that arise when an entry copy and a
/// cached copy of the same module both define the same names).
#[test]
fn opening_prelude_types_is_clean() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/core/prelude/types.wado";
        let source = include_str!("../../wado-compiler/lib/core/prelude/types.wado");
        let diags = diagnostics_for(path, source).await;
        let errs = errors(&diags);
        assert!(
            errs.is_empty(),
            "expected no errors, got {}: {:#?}",
            errs.len(),
            errs
        );
    });
}

/// Top-level prelude module — purely re-exports — must compile clean too.
#[test]
fn opening_prelude_root_is_clean() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/core/prelude.wado";
        let source = include_str!("../../wado-compiler/lib/core/prelude.wado");
        let diags = diagnostics_for(path, source).await;
        let errs = errors(&diags);
        assert!(
            errs.is_empty(),
            "expected no errors, got {}: {:#?}",
            errs.len(),
            errs
        );
    });
}

/// Same for a wasi interface (which transitively depends on prelude).
#[test]
fn opening_wasi_cli_stdout_is_clean() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/wasi/cli/stdout.wado";
        let source = include_str!("../../wado-compiler/lib/wasi/cli/stdout.wado");
        let diags = diagnostics_for(path, source).await;
        let errs = errors(&diags);
        assert!(
            errs.is_empty(),
            "expected no errors, got {}: {:#?}",
            errs.len(),
            errs
        );
    });
}

/// WASI flat-package roots (`lib/wasi/<pkg>.wado`) are pure re-export
/// hubs and carry both `#![generated]` and `#![stdlib("wasi:<pkg>")]`.
/// Cover the non-sub-interface code path so a regression in flat-package
/// identity resolution surfaces directly.
#[test]
fn opening_wasi_cli_flat_package_is_clean() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/wasi/cli.wado";
        let source = include_str!("../../wado-compiler/lib/wasi/cli.wado");
        let diags = diagnostics_for(path, source).await;
        let errs = errors(&diags);
        assert!(
            errs.is_empty(),
            "expected no errors, got {}: {:#?}",
            errs.len(),
            errs
        );
    });
}

/// Plain user code redefining `Option` does *not* opt out of the prelude
/// and must therefore still see the collision diagnostic.
#[test]
fn user_module_redefining_option_still_errors() {
    futures::executor::block_on(async {
        let path = "/work/myapp/main.wado";
        let source = "pub variant Option<T> { Some(T), None }\n";
        let diags = diagnostics_for(path, source).await;
        let saw_prelude_collision = diags
            .iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("prelude type"));
        assert!(
            saw_prelude_collision,
            "expected a prelude-collision error in user module, got: {diags:#?}",
        );
    });
}

/// User code that explicitly opts out via `#![no_prelude]` is allowed to
/// reuse prelude names — the collision check is keyed off the attribute,
/// not off the file's location or content.
#[test]
fn user_module_with_no_prelude_can_redefine_option() {
    futures::executor::block_on(async {
        let path = "/work/myapp/standalone.wado";
        let source = "#![no_prelude]\npub variant Option<T> { Some(T), None }\n";
        let diags = diagnostics_for(path, source).await;
        let collisions: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error && d.message.contains("prelude type"))
            .collect();
        assert!(
            collisions.is_empty(),
            "expected no prelude-collision errors with #![no_prelude], got: {collisions:#?}",
        );
    });
}

/// Effect diagnostics are produced from `Semantics`, so they surface in the
/// editor even though the LSP path builds no TIR. A call that needs an effect
/// the caller does not declare is reported, even in an uncalled function.
#[test]
fn effect_violation_is_reported() {
    futures::executor::block_on(async {
        let source = r#"
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
"#;
        let diags = diagnostics_for("/work/effect.wado", source).await;
        assert!(
            diags.iter().any(|d| d.message.contains("missing effect")),
            "expected an effect diagnostic, got {diags:#?}"
        );
    });
}

/// Stores violations are also produced from `Semantics`, so they surface in the
/// editor (the LSP runs `check_semantics`, which covers all three Design-B
/// checks — not just effects).
#[test]
fn stores_violation_is_reported() {
    futures::executor::block_on(async {
        let source = r#"
struct Data {
    value: i32,
}

fn bad_return(data: &Data) -> &Data {
    return data;
}

export fn run() {}
"#;
        let diags = diagnostics_for("/work/stores.wado", source).await;
        assert!(
            diags.iter().any(|d| d.message.contains("stores[data]")),
            "expected a stores diagnostic, got {diags:#?}"
        );
    });
}

/// Default-value purity violations likewise surface in the editor.
#[test]
fn purity_violation_is_reported() {
    futures::executor::block_on(async {
        let source = r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("x");
    return 1;
}

fn greet(value: i32 = noisy()) -> i32 {
    return value;
}

export fn run() {}
"#;
        let diags = diagnostics_for("/work/purity.wado", source).await;
        assert!(
            diags.iter().any(|d| d.message.contains("must be pure")),
            "expected a default-purity diagnostic, got {diags:#?}"
        );
    });
}

/// A bare call on a non-function value binding reports the callee as
/// not-callable *and* still resolves the arguments, so an error inside an
/// argument is not masked by the callee error.
#[test]
fn not_callable_callee_still_reports_argument_errors() {
    futures::executor::block_on(async {
        let source = concat!(
            "global X: i32 = 5;\n",
            "export fn run() {\n",
            "    let _ = X(undefined_fn());\n",
            "}\n",
        );
        let diags = diagnostics_for("/work/not_callable.wado", source).await;
        assert!(
            diags.iter().any(|d| d.message.contains("not callable")),
            "expected a not-callable diagnostic, got {diags:#?}"
        );
        assert!(
            diags.iter().any(|d| d.message.contains("undefined_fn")),
            "expected the argument's unknown-identifier diagnostic, got {diags:#?}"
        );
    });
}
