//! Integration tests for `Engine::diagnostics`.
//!
//! `Engine::diagnostics` runs only the `annotate` pipeline (parse → bind →
//! load → analyze → resolve), deliberately stopping before
//! codegen. These tests pin two contracts:
//!
//! - Bundled stdlib files carry `#![no_prelude]`, so opening them in an
//!   editor produces a clean compile (no `PreludeTypeCollision` against
//!   the types they themselves define for the prelude).
//! - User code that redefines a prelude name without opting out via
//!   `#![no_prelude]` still surfaces the collision diagnostic.
//! - Inputs that historically panicked during codegen validation now
//!   complete cleanly because `Engine::diagnostics` stops at `annotate`.

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic, SourceError};
use wado_lsp::{Diagnostic, Engine, Severity};

struct TestHost {
    sources: IndexMap<String, Vec<u8>>,
}

impl TestHost {
    fn empty() -> Self {
        Self {
            sources: IndexMap::new(),
        }
    }
}

impl CompilerHost for TestHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        if let Some(b) = self.sources.get(path) {
            return Ok(b.clone());
        }
        Err(SourceError::NotFound {
            path: path.to_string(),
        })
    }

    fn emit_diagnostic(&self, _diagnostic: CompilerDiagnostic) {}
}

async fn diagnostics_for(path: &str, source: &str) -> Vec<Diagnostic> {
    let uri = format!("file://{path}");
    let host = TestHost::empty();
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
