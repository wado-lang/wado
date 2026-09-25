//! A library export naming a record through an aliased `use`, directly and as
//! a field of the library's own record.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{
    WasiState, compile_source_with_compiler_options, engine, lib_func, limit_store, linker, runtime,
};

const LIB_WORLD_FQ: &str = "test:aliased/aliased@0.1.0";

const TIMEOUT_MS: u64 = 5_000;

const SOURCE: &str = r#"
use { Point as P } from "./sub/lib_aliased_point.wado";

pub struct Line {
    pub from: P,
    pub to: P,
}

export fn flip(p: P) -> P {
    return P { x: p.y, y: p.x };
}

export fn reverse(line: Line) -> Line {
    return Line { from: line.to, to: line.from };
}
"#;

fn point(x: i32, y: i32) -> Val {
    Val::Record(vec![("x".into(), Val::S32(x)), ("y".into(), Val::S32(y))])
}

fn line(from: Val, to: Val) -> Val {
    Val::Record(vec![("from".into(), from), ("to".into(), to)])
}

fn aliased_records_cross(opt_level: OptLevel) {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lib.wado");
    let wasm = compile_source_with_compiler_options(&path, SOURCE, options)
        .expect("library failed to compile")
        .wasm;
    let engine = engine();
    let component = Component::new(engine, &wasm).expect("component failed to load");

    runtime().block_on(async {
        let linker = linker(engine).expect("build linker");
        let mut store = Store::new(engine, WasiState::new());
        limit_store(&mut store, TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        for (name, arg, expected) in [
            ("flip", point(1, 2), point(2, 1)),
            (
                "reverse",
                line(point(1, 2), point(3, 4)),
                line(point(3, 4), point(1, 2)),
            ),
        ] {
            let func = lib_func(&mut store, &instance, LIB_WORLD_FQ, name);
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, &[arg], &mut results)
                .await
                .unwrap_or_else(|e| panic!("call {name}: {e}"));
            assert_eq!(results[0], expected, "{name}");
        }
    });
}

#[test]
fn aliased_records_cross_o0() {
    aliased_records_cross(OptLevel::O0);
}

#[test]
fn aliased_records_cross_o2() {
    aliased_records_cross(OptLevel::O2);
}
