//! `Engine::open_document_with_invocations`: a precomputed redirect index
//! (built by a runtime-backed host that ran the generators) drives the
//! `use { ... } from "<schema>"` redirect directly, bypassing consume-only
//! on-disk discovery.
//!
//! This pins the Engine contract that the native `wado query` path relies
//! on, independent of the CLI's wasmtime pipeline: the loader resolves a
//! `kiln:` redirect URI by stripping the scheme and calling
//! `CompilerHost::load_source`, so an in-memory `MapHost` serving the
//! generated module at that path is enough to exercise it.

use wado_compiler::kiln::InvocationIndex;
use wado_lsp::test_support::MapHost;
use wado_lsp::{Engine, Severity};

const ENTRY_URI: &str = "file:///entry.wado";
// Bare import, no inline `with` clause: consume-only discovery finds no
// invocation here, so only an injected index can make the redirect fire.
const ENTRY_SRC: &str = "use { parse } from \"./schema.g4\";\nfn run() { let _ = parse(); }\n";
const GENERATED: &str = "pub fn parse() -> i32 { return 1; }\n";

fn errors(diags: &[wado_lsp::Diagnostic]) -> Vec<&wado_lsp::Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

#[test]
fn injected_index_redirects_without_consume_only() {
    futures::executor::block_on(async {
        // The generated module is served at the kiln URI's stripped path.
        let host = MapHost::single("/gen/out.wado", GENERATED);
        let mut index = InvocationIndex::new();
        // `decl_file` is the entry filename the loader sees, i.e.
        // `Uri::to_filename(ENTRY_URI)`.
        index.insert("/entry.wado", "./schema.g4", "kiln:/gen/out.wado");

        let mut engine = Engine::new();
        engine.open_document_with_invocations(ENTRY_URI, ENTRY_SRC.to_string(), index);

        let diags = engine.diagnostics(ENTRY_URI, &host).await;
        assert!(
            errors(&diags).is_empty(),
            "the injected redirect should resolve `parse`, got {:#?}",
            errors(&diags),
        );
        assert!(
            !diags.iter().any(|d| d.code == "KILN_STALE_CACHE"),
            "an injected index must bypass consume-only discovery, got {diags:#?}",
        );
    });
}

#[test]
fn without_injection_the_bare_schema_import_fails() {
    futures::executor::block_on(async {
        // Same entry, no injected index and no inline `with` clause → no
        // redirect, and `./schema.g4` is not a real module. Proves the
        // injection above is what made the query resolve.
        let host = MapHost::empty();
        let mut engine = Engine::new();
        engine.open_document(ENTRY_URI, ENTRY_SRC.to_string());

        let diags = engine.diagnostics(ENTRY_URI, &host).await;
        assert!(
            !errors(&diags).is_empty(),
            "without the redirect, the schema import must fail to load",
        );
    });
}
