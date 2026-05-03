//! Integration tests for `Engine::diagnostics`. Focused on the cases where
//! the open document is itself a bundled stdlib source — the LSP must not
//! report `PreludeTypeCollision` / `DuplicateDefinition` against types that
//! the entry module legitimately defines as part of the prelude.

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

/// Opening `core:prelude/types.wado` (where Option, Result, … are defined)
/// must not surface duplicate-definition / prelude-collision errors against
/// the same prelude that gets implicitly loaded.
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

/// Same for a wasi interface (which transitively imports prelude).
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

/// Sanity check — a non-stdlib entry that *does* redefine a prelude type
/// must still surface the collision (the fix above must not silently
/// suppress all collisions).
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
