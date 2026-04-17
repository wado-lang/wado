//! Tests for the LSP-friendly `annotate` entry point.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::symbol::{SymbolKey, SymbolKind};
use wado_compiler::{ModuleSource, annotate};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

#[test]
fn annotate_indexes_struct_decl_by_symbol_key() {
    let source = r"
struct Point { x: i32, y: i32 }

export fn run() {
    let _p = Point { x: 1, y: 2 };
}
";
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();

    let entry = ModuleSource::EntryPoint {
        filename: "entry.wado".to_string(),
    };

    let point_symbol = annotated
        .symbols
        .lookup_in_module(&entry, "Point")
        .expect("Point symbol should be defined in entry module");
    assert!(matches!(point_symbol.kind, SymbolKind::Struct(_)));

    let key = SymbolKey::new(entry.clone(), point_symbol.defined_at.ast_id);

    let ty = annotated
        .type_at(&key)
        .expect("type_at(Point) should return a decl-backed ResolvedType");
    // The type exists; verifying the identity round-trip is enough — walking
    // back through `symbols_of_type` to the same `SymbolKey`.
    let type_id = annotated.types.type_of_symbol(&key).unwrap();
    let walked = annotated.types.symbol_of_type(type_id).unwrap();
    assert_eq!(walked.module, key.module);
    assert_eq!(walked.ast_id, key.ast_id);

    // The AST node behind the key is an innermost symbol-bearing node.
    let def = annotated
        .definition_of(&key)
        .expect("definition_of should resolve");
    assert_eq!(def.module, entry);
    assert_eq!(def.ast_id, key.ast_id);

    let _ = ty; // keep the borrow alive through the assertions above
}

#[test]
fn annotate_resolves_position_to_ast_id() {
    let source = "export fn run() {}\n";
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();

    let entry = ModuleSource::EntryPoint {
        filename: "entry.wado".to_string(),
    };

    // Column 12 is inside "run" on line 1.
    let id = annotated
        .ast_id_at(&entry, 1, 12)
        .expect("position inside `run` should resolve to an AstId");

    let run_symbol = annotated
        .symbols
        .lookup_in_module(&entry, "run")
        .expect("run symbol should be defined");
    assert_eq!(run_symbol.defined_at.ast_id, id);
}
