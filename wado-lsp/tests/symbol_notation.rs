//! Integration tests for `Engine::definition_by_symbol` — resolving a
//! Wado symbol notation (`MODULE#SYMBOL`) to a definition location.

use wado_compiler::symbol_notation;
use wado_lsp::test_support::MapHost;
use wado_lsp::{DefinitionResult, Engine, SymbolQueryError};

/// Resolve `notation` against a synthetic entry that imports `module_spec`,
/// with `lib_path`/`lib_src` served as the target module.
async fn resolve(
    lib_path: &str,
    lib_src: &str,
    module_spec: &str,
    notation: &str,
) -> Result<DefinitionResult, SymbolQueryError> {
    let entry_uri = "file:///__wado_query__.wado";
    let entry_src = format!("use __q from \"{module_spec}\";\n");
    let host = MapHost::with_files(&[(lib_path, lib_src), ("/__wado_query__.wado", &entry_src)]);
    let mut engine = Engine::new();
    engine.open_document(entry_uri, entry_src.clone());
    let parsed = symbol_notation::parse(notation).expect("notation parses");
    engine.definition_by_symbol(entry_uri, &parsed, &host).await
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
