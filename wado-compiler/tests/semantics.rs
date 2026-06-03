//! Tests for the LSP-friendly `semantics` entry point.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::{Semantics, semantics};
use wado_compiler::symbol::{SymbolKey, SymbolKind};

#[test]
fn semantics_newtype_records_aliased_type() {
    let source = r"
type Meters = f64;
type Pair = [i32, i32];
type Maybe = Option<i32>;
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let expect_aliased = |name: &str, expected: &str| {
        let sym = sem
            .symbols
            .lookup_in_module(&entry, name)
            .unwrap_or_else(|| panic!("{name} symbol should be defined"));
        match &sym.kind {
            SymbolKind::Newtype(n) => assert_eq!(n.aliased_type, expected, "newtype {name}"),
            other => panic!("expected Newtype for {name}, got {other:?}"),
        }
    };

    expect_aliased("Meters", "f64");
    expect_aliased("Pair", "[i32, i32]");
    expect_aliased("Maybe", "Option<i32>");
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

#[test]
fn semantics_indexes_struct_decl_by_symbol_key() {
    let source = r"
struct Point { x: i32, y: i32 }

export fn run() {
    let _p = Point { x: 1, y: 2 };
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let point_symbol = sem
        .symbols
        .lookup_in_module(&entry, "Point")
        .expect("Point symbol should be defined in entry module");
    assert!(matches!(point_symbol.kind, SymbolKind::Struct(_)));

    let key = SymbolKey::new(entry.clone(), point_symbol.defined_at.ast_id);

    let ty = sem
        .type_at(&key)
        .expect("type_at(Point) should return a decl-backed ResolvedType");
    // The type exists; verifying the identity round-trip is enough — walking
    // back through `symbols_of_type` to the same `SymbolKey`.
    let type_id = sem.types.type_of_symbol(&key).unwrap();
    let walked = sem.types.symbol_of_type(type_id).unwrap();
    assert_eq!(walked.module, key.module);
    assert_eq!(walked.ast_id, key.ast_id);

    // The AST node behind the key is an innermost symbol-bearing node.
    let def = sem
        .definition_of(&key)
        .expect("definition_of should resolve");
    assert_eq!(def.module, entry);
    assert_eq!(def.ast_id, key.ast_id);

    let _ = ty; // keep the borrow alive through the assertions above
}

#[test]
fn semantics_exposes_world_and_cm_interface_registries() {
    let source = "export fn run() {}\n";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    assert!(
        sem.is_complete(),
        "semantics should complete for a trivial program"
    );

    let world_registry = sem
        .world_registry()
        .expect("world_registry is populated when annotate completes");
    assert!(
        world_registry.has_world("wasi:cli/command"),
        "stdlib `wasi:cli/command` world should be registered on Semantics"
    );
    assert!(
        world_registry.has_world("wasi:http/service"),
        "stdlib `wasi:http/service` world should be registered on Semantics"
    );

    // Both registries come from the same `OnceLock`-cached
    // `build_from_stdlib` singleton, so the references the accessors
    // hand out are identical to a fresh `build_from_stdlib()` call.
    // This is what lets the WIT producer (Phase 1) and codegen treat
    // them as a single frontend-derived view.
    let (expected_cm_interface, expected_world) =
        wado_compiler::component_model::CmInterfaceRegistry::build_from_stdlib();
    let cm_interface_registry = sem
        .cm_interface_registry()
        .expect("cm_interface_registry is populated when annotate completes");
    assert!(
        std::ptr::eq(cm_interface_registry, expected_cm_interface),
        "cm_interface_registry accessor returns the build_from_stdlib singleton",
    );
    assert!(
        std::ptr::eq(world_registry, expected_world),
        "world_registry accessor returns the build_from_stdlib singleton",
    );
}

#[test]
fn semantics_resolves_position_to_ast_id() {
    let source = "export fn run() {}\n";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // Column 12 is inside "run" on line 1.
    let id = sem
        .ast_id_at(&entry, 1, 12)
        .expect("position inside `run` should resolve to an AstId");

    let run_symbol = sem
        .symbols
        .lookup_in_module(&entry, "run")
        .expect("run symbol should be defined");
    assert_eq!(run_symbol.defined_at.ast_id, id);
}

/// Verify that calls into stdlib resolve via the same `referenced_symbol`
/// edge whether or not the stdlib snapshot cache served the stdlib
/// module.  The semantics pipeline seeds `state.references` from the
/// snapshot's drained `references` map and the per-compile elaborator
/// walks the entry module's body to add the user-side use→def edges on
/// top — both halves are needed for the cross-module jump-to-def to
/// work.
#[test]
fn semantics_resolves_stdlib_call_to_stdlib_def() {
    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hello");
}
"#;
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // The `println` call site lives on line 5 ("    println(...)") at the
    // start of `println`.  Column 5 is the first character of the
    // identifier.
    let call_id = sem
        .ast_id_at(&entry, 5, 5)
        .expect("position inside `println` call should resolve to an AstId");
    let call_key = SymbolKey::new(entry, call_id);

    let def_key = sem
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

    let def_symbol = sem
        .symbol_at(&def_key)
        .expect("stdlib def must resolve to a Symbol via `symbol_at`");
    assert_eq!(def_symbol.name, "println");
}

/// Verify that the snapshot's locals don't leak into per-compile `Semantics::symbol_at`
/// lookups — the seeded `local_symbols` map only contributes stdlib-internal
/// keys, and resolving a user-defined `let` must hit the per-compile entry,
/// not anything carried over from the snapshot's empty entry source.
#[test]
fn semantics_resolves_user_let_binding_independently_of_snapshot() {
    let source = r"
export fn run() {
    let x = 1;
    let _y = x;
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // `let _y = x;` is on line 4.  Column 14 lands on the identifier `x`.
    let use_id = sem
        .ast_id_at(&entry, 4, 14)
        .expect("position inside `x` use should resolve to an AstId");
    let use_key = SymbolKey::new(entry.clone(), use_id);

    let def_key = sem
        .referenced_symbol(&use_key)
        .expect("`x` use site must record a use→def edge to the let binding");
    assert_eq!(
        def_key.module, entry,
        "user let binding must resolve to the per-compile entry, got {:?}",
        def_key.module,
    );
    let def_symbol = sem
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
fn semantics_references_are_stable_across_cached_compiles() {
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
    let resolve_println_def = |a: &Semantics| -> (SymbolKey, String) {
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

    let a1 = block_on(semantics(source, &host, Some("entry.wado")));
    let r1 = resolve_println_def(&a1);

    let a2 = block_on(semantics(source, &host, Some("entry.wado")));
    let r2 = resolve_println_def(&a2);

    assert_eq!(
        r1, r2,
        "println use→def edge must be identical between cold and cached compiles"
    );
    assert_eq!(r1.1, "println");
}

/// Stage 4 of the elaborator re-architecture WEP: every `MethodCallExpr`
/// the body walk reaches must leave its dispatch decision in
/// `ModuleSemantics::types.method_dispatch`, surfaced through
/// `Semantics::method_dispatch_view`. The synthetic helper calls used by
/// the for-of loop carry `call_id == None` and stay out of the map.
#[test]
fn semantics_records_method_dispatch_per_call_site() {
    let source = r"
export fn run() {
    let xs: List<i32> = [1, 2, 3];
    let _n = xs.len();
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // Walk every recorded dispatch and check that one of them is the
    // `xs.len()` call. Using `iter_method_dispatch` here (rather than a
    // brittle column lookup that depends on which sub-node sits beneath
    // the cursor) keeps the test resilient to AstId numbering changes.
    let hit = sem.iter_method_dispatch().any(|key| {
        key.module == entry
            && sem
                .method_dispatch_view(key)
                .is_some_and(|(name, _, self_kind)| name.contains("len") && self_kind == "ref")
    });
    assert!(
        hit,
        "the `xs.len()` MethodCallExpr must record a dispatch decision with self_kind=ref",
    );
}

/// Stage 4 of the elaborator re-architecture WEP: each TIR-direct
/// desugar site (`assert`, `matches`, comparison chain, for-of, `while`,
/// compound assignment) records its variant in
/// `ModuleSemantics::types.desugars`. Verify a few representative kinds
/// land in the map for a small program that exercises them.
#[test]
fn semantics_records_desugar_kind_per_ast_id() {
    // One fixture per `DesugarKind` variant, so a future regression
    // that drops `record_desugar` from any of the surface sites is
    // caught by this single test. Each helper expression below targets
    // exactly one variant. See `crate::elaborator::sem::types::DesugarKind`.
    let source = r"
fn helper() -> Option<i32> {
    return Option::Some(1);
}

export fn run() {
    // Assert
    assert 1 == 1;

    // CompoundAssign
    let mut x = 0;
    x += 1;

    // While (Condition::Expr)
    while x < 5 {
        x = x + 1;
    }

    // WhileLetChain (Condition::LetChain)
    while let Option::Some(v) = helper() {
        x = v;
        break;
    }

    // CStyleFor (no parentheses around the header)
    for let mut i = 0; i < 3; i += 1 {
        x = x + i;
    }

    // ForOfIterator (List implements IntoIterator)
    let xs: List<i32> = [10, 20, 30];
    for let v of xs {
        x = x + v;
    }

    // ForOfTuple (tuple iteration — compile-time expansion)
    for let v of [10, 20, 30] {
        x = x + v;
    }

    // ComparisonChain (only triggered with 2+ comparisons)
    let _chained = 1 < 2 < 3;

    // Matches
    let _m = Option::Some(1) matches { Option::Some(_) };

    // IfLetChain (Condition::LetChain on if)
    if let Option::Some(_v) = helper() {
        x = x + 1;
    }
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let mut kinds: Vec<String> = sem
        .iter_desugars()
        .filter(|key| key.module == entry)
        .filter_map(|key| sem.desugar_view(key))
        .collect();
    kinds.sort();
    kinds.dedup();
    let expected = [
        "assert",
        "c_style_for",
        "comparison_chain",
        "compound_assign",
        "for_of_iterator",
        "for_of_tuple",
        "if_let_chain",
        "matches",
        "while",
        "while_let_chain",
    ];
    for want in &expected {
        assert!(
            kinds.iter().any(|k| k == want),
            "expected desugar kind {want} in {kinds:?}",
        );
    }
}

/// Stage 4 of the elaborator re-architecture WEP: every successful
/// branch of `try_coerce` records its [`CoercionKind`] on
/// `ModuleSemantics::types.coercions`. Verify the numeric-literal
/// (`1 → u32`) and null-to-option (`null → Option<i32>`) variants both
/// land in the map.
#[test]
fn semantics_records_coercion_choice_per_ast_id() {
    let source = r"
export fn run() {
    let _x: u32 = 1;
    let _y: Option<i32> = null;
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let mut saw_numeric = false;
    let mut saw_null_to_option = false;
    for key in sem.iter_coercions() {
        if key.module != entry {
            continue;
        }
        let Some((kind, _target)) = sem.coercion_view(key) else {
            continue;
        };
        if kind == "numeric_literal" {
            saw_numeric = true;
        }
        if kind == "null_to_option" {
            saw_null_to_option = true;
        }
    }
    assert!(
        saw_numeric,
        "`1 → u32` must record a numeric_literal coercion"
    );
    assert!(
        saw_null_to_option,
        "`null → Option<i32>` must record a null_to_option coercion",
    );
}

/// Stage 4 of the elaborator re-architecture WEP: every expression
/// visited by the body walk must leave its resolved [`TypeId`] in
/// [`Semantics::expression_types`], keyed by the expression's
/// `(module, AstId)`. Verify a few representative sub-expressions
/// (literal, binary op, identifier) all land in the map.
#[test]
fn semantics_records_expression_type_per_ast_id() {
    let source = r"
export fn run() {
    let x = 1 + 2;
    let _y = x;
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // Line 3: "    let x = 1 + 2;"
    //          12345678901234567890
    //                       ^c13  ^c17
    // Column 13 lands on the `1` literal; column 17 lands on the `2`
    // literal; the surrounding `1 + 2` binary covers both, so an id
    // resolved between them should also have an annotation.
    let one_id = sem
        .ast_id_at(&entry, 3, 13)
        .expect("position on `1` literal should resolve to an AstId");
    let one_key = SymbolKey::new(entry.clone(), one_id);
    let one_ty = sem
        .expression_type(&one_key)
        .expect("the `1` literal must record an expression type");
    assert_eq!(sem.types.type_name(one_ty), "i32");

    // Line 4: "    let _y = x;"
    //                     ^c14
    let x_use_id = sem
        .ast_id_at(&entry, 4, 14)
        .expect("position on `x` use should resolve to an AstId");
    let x_use_key = SymbolKey::new(entry, x_use_id);
    let x_ty = sem
        .expression_type(&x_use_key)
        .expect("the `x` use site must record an expression type");
    assert_eq!(sem.types.type_name(x_ty), "i32");
}

/// Stage 4 / WEP 2026-05-26: `try_coerce_*` sub-helpers record the
/// coercion at the decision point, so the callers that bypass
/// `try_coerce` (the `as`-cast path in `resolve_cast`, the struct
/// literal target in `resolve_let`, the deferred coercion fixup for
/// generic struct fields, and `recoerce_literal_args` after type-arg
/// inference) still leave a coercion entry. Verify a representative
/// `[1, 2, 3] as List<i32>` cast records `tuple_to_sequence`.
#[test]
fn semantics_records_coercion_through_cast_bypass() {
    let source = r"
export fn run() {
    let _xs = [10, 20, 30] as List<i32>;
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let saw_tuple_to_sequence = sem.iter_coercions().any(|key| {
        key.module == entry
            && sem
                .coercion_view(key)
                .is_some_and(|(kind, _)| kind == "tuple_to_sequence")
    });
    assert!(
        saw_tuple_to_sequence,
        "`[1, 2, 3] as List<i32>` must record a tuple_to_sequence coercion",
    );
}

/// Stage 4 / WEP 2026-05-26: post-inference `recoerce_literal_args`
/// re-coerces generic literal arguments after the type parameter is
/// resolved; the re-coercion must update `expression_types` so the map
/// matches the TIR's resolved type (otherwise reify would emit the
/// pre-inference default i32 instead of the inferred type).
#[test]
fn semantics_recoerce_literal_args_updates_expression_type() {
    let source = r"
fn two<T>(a: T, b: T) -> T {
    return a;
}

export fn run() {
    let _v = two::<i64>(1, 2);
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // Every recorded i32 literal in the user module would be a regression
    // (the only literals are the two `1` / `2` args, which must surface as
    // i64 after type-arg inference).
    let any_i32 = sem
        .expression_types
        .iter()
        .any(|(key, &type_id)| key.module == entry && sem.types.type_name(type_id) == "i32");
    assert!(
        !any_i32,
        "post-inference recoerce_literal_args must overwrite the pre-inference i32 entry",
    );
    let saw_i64 = sem
        .expression_types
        .iter()
        .any(|(key, &type_id)| key.module == entry && sem.types.type_name(type_id) == "i64");
    assert!(
        saw_i64,
        "post-inference recoerce_literal_args must record the inferred i64 type",
    );
}

/// Stage 4 / WEP 2026-05-26: failed method lookup emits a `MethodNotFound`
/// diagnostic and falls through with a placeholder `MethodInfo` so error
/// recovery can continue. The dispatch-recording gate must skip writing
/// to `method_dispatch` in this case — recording a `FunctionRef` whose
/// mangled name targets a non-existent method would mislead Stage 5
/// reify into lowering a call to a function that does not exist.
#[test]
fn semantics_skips_method_dispatch_when_lookup_failed() {
    let source = r"
export fn run() {
    let xs: List<i32> = [1, 2, 3];
    let _ = xs.no_such_method();
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    let saw_bogus = sem.iter_method_dispatch().any(|key| {
        key.module == entry
            && sem
                .method_dispatch_view(key)
                .is_some_and(|(name, _, _)| name.contains("no_such_method"))
    });
    assert!(
        !saw_bogus,
        "the MethodNotFound error path must not leave a bogus dispatch entry",
    );
}

/// Stage 4 / WEP 2026-05-26: top-level `match` at statement position
/// goes through `resolve_match_expr` directly (not `resolve_expr`), so
/// the stmt arm must explicitly record `expression_types` for the match
/// to keep the per-AstId annotation map populated.
#[test]
fn semantics_records_expression_type_for_stmt_position_match() {
    let source = r"
export fn run() {
    let x = Some(1);
    match x {
        Some(_) => {},
        None => {},
    }
}
";
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));

    let entry = sem.interner.borrow_mut().entry_point("entry.wado");

    // Line 4 column 5 lands on the `match` keyword.
    let match_id = sem
        .ast_id_at(&entry, 4, 5)
        .expect("`match` keyword should resolve to an AstId");
    let match_key = SymbolKey::new(entry, match_id);
    let match_ty = sem
        .expression_type(&match_key)
        .expect("stmt-position match must record an expression type");
    assert_eq!(sem.types.type_name(match_ty), "()");
}
