//! Tests for the LSP-friendly `annotate` entry point.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::annotate;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::symbol::{SymbolKey, SymbolKind};

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

    let entry = annotated.interner.borrow_mut().entry_point("entry.wado");

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

    let entry = annotated.interner.borrow_mut().entry_point("entry.wado");

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

/// Verify that calls into stdlib resolve via the same `referenced_symbol`
/// edge whether or not the stdlib snapshot cache served the stdlib
/// module.  The annotate pipeline seeds `state.references` from the
/// snapshot's drained `references` map and the per-compile resolver
/// walks the entry module's body to add the user-side use→def edges on
/// top — both halves are needed for the cross-module jump-to-def to
/// work.
#[test]
fn annotate_resolves_stdlib_call_to_stdlib_def() {
    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello");
}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();

    let entry = annotated.interner.borrow_mut().entry_point("entry.wado");

    // The `println` call site lives on line 5 ("    println(...)") at the
    // start of `println`.  Column 5 is the first character of the
    // identifier.
    let call_id = annotated
        .ast_id_at(&entry, 5, 5)
        .expect("position inside `println` call should resolve to an AstId");
    let call_key = SymbolKey::new(entry.clone(), call_id);

    let def_key = annotated
        .referenced_symbol(&call_key)
        .expect("`println` call site must record a use→def edge to the stdlib decl");
    // The defining symbol lives in a stdlib module — either `core:cli`
    // (where the user imports from) or a re-exported origin.
    assert!(
        matches!(
            &def_key.module,
            ModuleSource::Core { .. } | ModuleSource::Wasi { .. }
        ),
        "println def should live in stdlib, got {:?}",
        def_key.module,
    );

    let def_symbol = annotated
        .symbol_at(&def_key)
        .expect("stdlib def must resolve to a Symbol via `symbol_at`");
    assert_eq!(def_symbol.name, "println");
}

/// Verify that the snapshot's locals don't leak into per-compile `Annotated::symbol_at`
/// lookups — the seeded `local_symbols` map only contributes stdlib-internal
/// keys, and resolving a user-defined `let` must hit the per-compile entry,
/// not anything carried over from the snapshot's empty entry source.
#[test]
fn annotate_resolves_user_let_binding_independently_of_snapshot() {
    let source = r"
export fn run() {
    let x = 1;
    let _y = x;
}
";
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();

    let entry = annotated.interner.borrow_mut().entry_point("entry.wado");

    // `let _y = x;` is on line 4.  Column 14 lands on the identifier `x`.
    let use_id = annotated
        .ast_id_at(&entry, 4, 14)
        .expect("position inside `x` use should resolve to an AstId");
    let use_key = SymbolKey::new(entry.clone(), use_id);

    let def_key = annotated
        .referenced_symbol(&use_key)
        .expect("`x` use site must record a use→def edge to the let binding");
    assert_eq!(
        def_key.module, entry,
        "user let binding must resolve to the per-compile entry, got {:?}",
        def_key.module,
    );
    let def_symbol = annotated
        .symbol_at(&def_key)
        .expect("user let binding must resolve via `symbol_at` (locals table)");
    assert_eq!(def_symbol.name, "x");
}

/// Compile the same source twice on the same thread to force a snapshot
/// cache hit on the second call, then verify the user→stdlib edge
/// resolves to the same definition both times.  Divergence here would
/// mean the seeded `references` map drifts when a stdlib module is
/// served from the snapshot rather than freshly resolved.
#[test]
fn annotate_references_are_stable_across_cached_compiles() {
    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let msg = "hi";
    println(msg);
}
"#;
    let host = InMemoryHost::new();

    // Resolve the same use→def edge under both a cold and a (potentially
    // cached) compile and require they agree exactly.  We compare both
    // the resolved `SymbolKey` and the underlying `Symbol::name`, since
    // a stale snapshot could in principle yield a different key with
    // the same name.
    let resolve_println_def = |a: &wado_compiler::annotate::Annotated| -> (SymbolKey, String) {
        let entry = a.interner.borrow_mut().entry_point("entry.wado");
        // Line 6: `    println(msg);` — column 5 lands on `println`.
        let call_id = a
            .ast_id_at(&entry, 6, 5)
            .expect("println call should resolve to an AstId");
        let use_key = SymbolKey::new(entry, call_id);
        let def_key = a
            .referenced_symbol(&use_key)
            .expect("println call must record a use→def edge");
        let name = a
            .symbol_at(&def_key)
            .expect("def must resolve to a Symbol")
            .name
            .clone();
        (def_key, name)
    };

    let a1 = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let r1 = resolve_println_def(&a1);

    let a2 = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let r2 = resolve_println_def(&a2);

    assert_eq!(
        r1, r2,
        "println use→def edge must be identical between cold and cached compiles"
    );
    assert_eq!(r1.1, "println");
}
