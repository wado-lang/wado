//! Integration tests for `Engine::diagnostics`.
//!
//! `Engine::diagnostics` runs only the `annotate` pipeline (parse → bind →
//! desugar → load → analyze → resolve), deliberately stopping before
//! codegen. These tests pin the resulting contract:
//!
//! - Inputs that cause downstream phases to panic (notably codegen
//!   validation on unusual entry modules) must not propagate that panic to
//!   the LSP layer.
//! - User-actionable diagnostics that surface during annotation —
//!   prelude-name collisions, undefined symbols — are still reported.

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

/// Bundled stdlib sources that, when fed to the full compile pipeline as the
/// entry module, panic during codegen validation (the WIR emitted for them
/// is not a valid component because they expose stdlib internals rather
/// than a runnable world). `Engine::diagnostics` must complete without
/// panicking by stopping at `annotate`.
#[test]
fn opening_prelude_types_does_not_panic() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/core/prelude/types.wado";
        let source = include_str!("../../wado-compiler/lib/core/prelude/types.wado");
        let _ = diagnostics_for(path, source).await;
    });
}

#[test]
fn opening_wasi_cli_stdout_does_not_panic() {
    futures::executor::block_on(async {
        let path = "/work/wado-compiler/lib/wasi/cli/stdout.wado";
        let source = include_str!("../../wado-compiler/lib/wasi/cli/stdout.wado");
        let _ = diagnostics_for(path, source).await;
    });
}

/// A user module that redefines a prelude type still surfaces the
/// collision diagnostic. Guards against accidentally suppressing legitimate
/// errors when the LSP layer is reworked.
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
