//! Lift/lower binding test for Component Model value types outside any WASI
//! world.
//!
//! `tests/fixtures/cm_catalog.wado` enumerates the CM value-type ABI surface as
//! `identity` exports (each returns its argument unchanged). This harness
//! compiles that fixture as a `--lib` (a synthesized library world, the
//! `wado compile --lib` path), instantiates the component, and round-trips a
//! crafted [`Val`] through every `id_*` export, asserting `lift(lower(x)) == x`.
//!
//! Because the exports are identities, equality of the returned value with the
//! input is the whole oracle — one data-driven table covers the entire value
//! surface without per-type Rust bindings.
//!
//! The same fixture is also discovered by `e2e.rs` under the test world (it
//! carries an empty `test` block); that run only confirms a library-shaped
//! source compiles and instantiates. The lift/lower assertions live here.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Val};

/// FQ of the synthesized library world. Mirrors `lib_world_fq` in
/// `wado-cli`: `namespace:name/name@version`.
const LIB_WORLD_FQ: &str = "wado:cm-catalog/cm-catalog@0.1.0";

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cm_catalog.wado"
);

/// A single round-trip case: call the kebab-named export with `value` and
/// assert the returned value equals `value`.
struct Case {
    export: &'static str,
    value: Val,
}

fn case(export: &'static str, value: Val) -> Case {
    Case { export, value }
}

fn b(v: Val) -> Option<Box<Val>> {
    Some(Box::new(v))
}

/// The full value-type catalog as round-trip cases. Mirrors the `id_*` exports
/// in `tests/fixtures/cm_catalog.wado`.
fn cases() -> Vec<Case> {
    let point = || {
        Val::Record(vec![
            ("x".into(), Val::Float64(1.5)),
            ("y".into(), Val::Float64(-2.5)),
        ])
    };
    vec![
        // Primitives.
        case("id-bool", Val::Bool(true)),
        case("id-u8", Val::U8(0xAB)),
        case("id-u16", Val::U16(0xBEEF)),
        case("id-u32", Val::U32(0xDEAD_BEEF)),
        case("id-u64", Val::U64(0x0123_4567_89AB_CDEF)),
        case("id-s8", Val::S8(-12)),
        case("id-s16", Val::S16(-1234)),
        case("id-s32", Val::S32(-123_456)),
        case("id-s64", Val::S64(-1_234_567_890_123)),
        case("id-f32", Val::Float32(3.5)),
        case("id-f64", Val::Float64(-7.25)),
        case("id-char", Val::Char('λ')),
        case("id-string", Val::String("héllo, wörld".into())),
        // Built-in containers.
        case(
            "id-list-u8",
            Val::List(vec![Val::U8(1), Val::U8(2), Val::U8(3)]),
        ),
        case(
            "id-list-string",
            Val::List(vec![Val::String("a".into()), Val::String("bb".into())]),
        ),
        case(
            "id-tuple-pair",
            Val::Tuple(vec![Val::U32(7), Val::String("x".into())]),
        ),
        case(
            "id-tuple-triple",
            Val::Tuple(vec![Val::U8(9), Val::Bool(true), Val::Char('z')]),
        ),
        case("id-option-u32", Val::Option(b(Val::U32(42)))),
        case("id-option-string", Val::Option(None)),
        // `result` in all four WIT forms.
        case("id-result-both", Val::Result(Ok(b(Val::U32(5))))),
        case("id-result-ok", Val::Result(Ok(b(Val::U32(6))))),
        case(
            "id-result-err",
            Val::Result(Err(b(Val::String("boom".into())))),
        ),
        case("id-result-unit", Val::Result(Ok(None))),
        // Named value types.
        case("id-record", point()),
        case("id-enum", Val::Enum("green".into())),
        case(
            "id-variant",
            Val::Variant("circle".into(), b(Val::Float64(4.0))),
        ),
        case(
            "id-flags",
            Val::Flags(vec!["read".into(), "execute".into()]),
        ),
        case("id-newtype", Val::Float64(100.0)),
        // Mixed-core-class variant: the `as-float` arm's f32 payload shares a
        // slot the join widened to i32, so it must be bit-reinterpreted.
        case(
            "id-mixed",
            Val::Variant("as-float".into(), b(Val::Float32(2.5))),
        ),
        case("id-mixed", Val::Variant("as-int".into(), b(Val::U32(7)))),
        // Mixed-core-class result: the `err` arm's u32 shares an f32-widened slot.
        case("id-result-fu", Val::Result(Err(b(Val::U32(42))))),
        case("id-result-fu", Val::Result(Ok(b(Val::Float32(1.5))))),
        // Nested compositions.
        case(
            "id-list-option",
            Val::List(vec![Val::Option(b(Val::U32(1))), Val::Option(None)]),
        ),
        case(
            "id-option-list",
            Val::Option(b(Val::List(vec![Val::U8(1), Val::U8(2)]))),
        ),
        case("id-list-record", Val::List(vec![point(), point()])),
        case("id-option-record", Val::Option(b(point()))),
        case(
            "id-list-tuple",
            Val::List(vec![Val::Tuple(vec![Val::U32(1), Val::String("a".into())])]),
        ),
        case(
            "id-result-list",
            Val::Result(Ok(b(Val::List(vec![point()])))),
        ),
        // Association list (the CM map idiom `list<tuple<k, v>>`).
        case(
            "id-assoc-array",
            Val::List(vec![Val::Tuple(vec![Val::String("k".into()), Val::U32(1)])]),
        ),
        // Tuples carrying aggregate (non-primitive) elements.
        case("id-tuple-record", Val::Tuple(vec![point(), Val::U32(7)])),
        case(
            "id-tuple-option",
            Val::Tuple(vec![Val::Option(b(Val::U32(9))), Val::U8(3)]),
        ),
        case(
            "id-tuple-list",
            Val::Tuple(vec![
                Val::List(vec![Val::String("a".into()), Val::String("bb".into())]),
                Val::U32(5),
            ]),
        ),
        case(
            "id-list-tuple-record",
            Val::List(vec![
                Val::Tuple(vec![point(), Val::U32(1)]),
                Val::Tuple(vec![point(), Val::U32(2)]),
            ]),
        ),
    ]
}

/// Compile the catalog fixture as a library world at `opt_level`.
fn compile_catalog(opt_level: OptLevel) -> Vec<u8> {
    let source = std::fs::read_to_string(FIXTURE).expect("read cm_catalog fixture");
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        // The debug allocator poisons freed memory, surfacing lift/lower
        // use-after-free at the boundary.
        allocator: Some("debug".to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new(FIXTURE), &source, options)
        .expect("catalog failed to compile as a library world")
        .wasm
}

/// Round-trip every case through the compiled component, asserting identity.
fn run_round_trips(opt_level: OptLevel) {
    let wasm = compile_catalog(opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        // The value-type library still imports the prelude's `wasi:cli/stderr`
        // (assertion/format diagnostics), so use the shared WASI linker.
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        // The shared engine enables epoch interruption; without a deadline the
        // first call traps with `interrupt`.
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        // Named-type exports group into the default interface; resolve the
        // `wado:cm-catalog/cm-catalog@…` instance once and look funcs up inside.
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);

        let mut failures = Vec::new();
        for Case { export, value } in cases() {
            let func = iface
                .as_ref()
                .and_then(|i| instance.get_export(&mut store, Some(i), export))
                .map(|(_, idx)| idx)
                .and_then(|idx| instance.get_func(&mut store, idx));
            let Some(func) = func else {
                failures.push(format!("[{opt}] export `{export}` not found"));
                continue;
            };
            let result_arity = func.ty(&store).results().len();
            let mut results = vec![Val::Bool(false); result_arity];
            match func.call_async(&mut store, &[value.clone()], &mut results).await {
                Ok(()) => {
                    if results.len() != 1 {
                        failures.push(format!(
                            "[{opt}] `{export}`: expected 1 result, got {}",
                            results.len()
                        ));
                    } else if results[0] != value {
                        failures.push(format!(
                            "[{opt}] `{export}`: round-trip mismatch\n  in:  {value:?}\n  out: {:?}",
                            results[0]
                        ));
                    }
                }
                Err(e) => failures.push(format!("[{opt}] `{export}`: call trapped: {e:#}")),
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    });
}

/// Compile an inline library source at O0, returning the compiler's result.
fn try_compile_lib(source: &str) -> Result<(), String> {
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A library export whose signature carries a type with no Component Model value
/// representation must be rejected with a readable compile error, not an ICE or
/// silently-wrong code. An empty record is the canonical case.
#[test]
fn cm_lib_rejects_empty_record_boundary_type() {
    let err = try_compile_lib(
        "pub struct Empty {}\nexport fn id_empty(v: Empty) -> Empty {\n    return v;\n}\n",
    )
    .expect_err("empty-record export should fail to compile");
    assert!(
        err.contains("id_empty") && err.contains("empty record"),
        "expected an empty-record boundary diagnostic, got: {err}"
    );
}

#[test]
fn cm_catalog_round_trip_o0() {
    run_round_trips(OptLevel::O0);
}

#[test]
fn cm_catalog_round_trip_o2() {
    run_round_trips(OptLevel::O2);
}
