//! Integration tests for `Engine::definition_by_symbol` — resolving a
//! Wado symbol notation (`MODULE#SYMBOL`) to a definition location.

use wado_compiler::symbol_notation;
use wado_lsp::test_support::MapHost;
use wado_lsp::{DefinitionResult, DocumentHighlight, Engine, ReferenceLocation, SymbolQueryError};

const ENTRY_URI: &str = "file:///__wado_query__.wado";

/// Build an engine over a synthetic entry that imports `module_spec`, serving
/// `files` (plus the entry) through the host.
fn engine_for(files: &[(&str, &str)], module_spec: &str) -> (Engine, MapHost) {
    let entry_src = format!("use __q from \"{module_spec}\";\n");
    let mut all: Vec<(&str, &str)> = files.to_vec();
    let leaked: &'static str = Box::leak(entry_src.into_boxed_str());
    all.push(("/__wado_query__.wado", leaked));
    let host = MapHost::with_files(&all);
    let mut engine = Engine::new();
    engine.open_document(ENTRY_URI, leaked.to_string());
    (engine, host)
}

/// Resolve `notation` against a synthetic entry that imports `module_spec`,
/// with `lib_path`/`lib_src` served as the target module.
async fn resolve(
    lib_path: &str,
    lib_src: &str,
    module_spec: &str,
    notation: &str,
) -> Result<DefinitionResult, SymbolQueryError> {
    let (engine, host) = engine_for(&[(lib_path, lib_src)], module_spec);
    let parsed = symbol_notation::parse(notation).expect("notation parses");
    engine.definition_by_symbol(ENTRY_URI, &parsed, &host).await
}

async fn references(
    files: &[(&str, &str)],
    module_spec: &str,
    notation: &str,
    include_declaration: bool,
) -> Result<Vec<ReferenceLocation>, SymbolQueryError> {
    let (engine, host) = engine_for(files, module_spec);
    let parsed = symbol_notation::parse(notation).expect("notation parses");
    engine
        .references_by_symbol(ENTRY_URI, &parsed, include_declaration, &host)
        .await
}

async fn highlights(
    lib_path: &str,
    lib_src: &str,
    module_spec: &str,
    notation: &str,
) -> Result<(String, Vec<DocumentHighlight>), SymbolQueryError> {
    let (engine, host) = engine_for(&[(lib_path, lib_src)], module_spec);
    let parsed = symbol_notation::parse(notation).expect("notation parses");
    engine
        .document_highlight_by_symbol(ENTRY_URI, &parsed, &host)
        .await
}

#[test]
fn free_function_in_local_module() {
    futures::executor::block_on(async {
        let lib = "pub fn helper() -> i32 { return 1; }\n";
        let result = resolve("./lib.wado", lib, "./lib.wado", "./lib.wado#helper")
            .await
            .expect("resolves free function");
        assert_eq!(result.uri, "file:///lib.wado");
        // `helper` name span: codepoints 7..13 on line 0.
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 7);
        assert_eq!(result.range.end.character, 13);
    });
}

#[test]
fn type_in_local_module() {
    futures::executor::block_on(async {
        let lib = "pub struct Point { x: i32, y: i32 }\n";
        let result = resolve("./geo.wado", lib, "./geo.wado", "./geo.wado#Point")
            .await
            .expect("resolves struct type");
        assert_eq!(result.uri, "file:///geo.wado");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 11);
        assert_eq!(result.range.end.character, 16);
    });
}

#[test]
fn unknown_symbol_lists_public_symbols() {
    futures::executor::block_on(async {
        let lib = concat!(
            "pub fn helper() -> i32 { return 1; }\n",
            "pub struct Widget { x: i32 }\n",
            "fn private_helper() -> i32 { return 2; }\n",
        );
        let err = resolve("./lib.wado", lib, "./lib.wado", "./lib.wado#missing")
            .await
            .expect_err("missing symbol is an error");
        match err {
            SymbolQueryError::NotFound { available } => {
                // Public items are suggested; private ones are not.
                assert_eq!(available, vec!["Widget".to_string(), "helper".to_string()]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    });
}

#[test]
fn references_finds_use_sites() {
    futures::executor::block_on(async {
        let lib = concat!(
            "pub fn helper() -> i32 { return 1; }\n",
            "pub fn use_it() -> i32 { return helper(); }\n",
        );
        let refs = references(
            &[("./lib.wado", lib)],
            "./lib.wado",
            "./lib.wado#helper",
            false,
        )
        .await
        .expect("resolves references");
        // The call site inside `use_it` on line 1, codepoints 32..38.
        assert_eq!(refs.len(), 1, "got: {refs:?}");
        assert_eq!(refs[0].uri, "file:///lib.wado");
        assert_eq!(refs[0].range.start.line, 1);
        assert_eq!(refs[0].range.start.character, 32);
        assert_eq!(refs[0].range.end.character, 38);
    });
}

#[test]
fn references_include_declaration() {
    futures::executor::block_on(async {
        let lib = concat!(
            "pub fn helper() -> i32 { return 1; }\n",
            "pub fn use_it() -> i32 { return helper(); }\n",
        );
        let refs = references(
            &[("./lib.wado", lib)],
            "./lib.wado",
            "./lib.wado#helper",
            true,
        )
        .await
        .expect("resolves references");
        // Declaration (0, 7..13) then the call (1, 32..38).
        let ranges: Vec<(u32, u32)> = refs
            .iter()
            .map(|r| (r.range.start.line, r.range.start.character))
            .collect();
        assert_eq!(ranges, vec![(0, 7), (1, 32)]);
    });
}

#[test]
fn document_highlight_classifies_occurrences() {
    futures::executor::block_on(async {
        let lib = concat!(
            "pub fn helper() -> i32 { return 1; }\n",
            "pub fn use_it() -> i32 { return helper(); }\n",
        );
        let (def_uri, hl) = highlights("./lib.wado", lib, "./lib.wado", "./lib.wado#helper")
            .await
            .expect("resolves highlights");
        assert_eq!(def_uri, "file:///lib.wado");
        use wado_lsp::HighlightKind;
        let summary: Vec<(u32, u32, HighlightKind)> = hl
            .iter()
            .map(|h| (h.range.start.line, h.range.start.character, h.kind))
            .collect();
        assert_eq!(
            summary,
            vec![(0, 7, HighlightKind::Write), (1, 32, HighlightKind::Read),]
        );
    });
}

#[test]
fn method_notation_is_unsupported_for_now() {
    futures::executor::block_on(async {
        let lib =
            "pub struct Point { x: i32 }\nimpl Point { pub fn zero() -> i32 { return 0; } }\n";
        let err = resolve("./geo.wado", lib, "./geo.wado", "./geo.wado#Point::zero")
            .await
            .expect_err("associated function resolution not yet supported");
        assert_eq!(err, SymbolQueryError::Unsupported);
    });
}
