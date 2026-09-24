//! A `--lib` export signature names the declaration a type reaches, not its
//! spelling: a struct imported as `u32` crosses as the struct's record.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{
    DEFAULT_TIMEOUT_MS, WasiState, compile_source_with_compiler_options, engine, lib_func,
    limit_store, linker, runtime,
};

const LIB_WORLD_FQ: &str = "test:alias/alias@0.1.0";

const ALIASED: &str = r#"
use { Foo as u32 } from "./sub/alias_user_struct_as_primitive_dep.wado";

export fn take(x: u32) -> i32 {
    return x.v;
}

export fn make(v: i32) -> u32 {
    return u32 { v };
}
"#;

const UNALIASED: &str = r#"
use { Foo } from "./sub/alias_user_struct_as_primitive_dep.wado";

export fn take(x: Foo) -> i32 {
    return x.v;
}

export fn make(v: i32) -> Foo {
    return Foo { v };
}
"#;

fn compile(source: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lib.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("library compiles")
        .wasm
}

/// `take`'s parameter and `make`'s result, as the component's WIT names them.
fn signature_type_names(wasm: &[u8]) -> [String; 2] {
    let wit_component::DecodedWasm::Component(resolve, world) =
        wit_component::decode(wasm).expect("decode component")
    else {
        panic!("expected a component");
    };
    let func = |name: &str| {
        resolve.worlds[world]
            .exports
            .iter()
            .find_map(|(_, item)| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].functions.get(name).cloned()
                }
                wit_parser::WorldItem::Function(f) if f.name == name => Some(f.clone()),
                wit_parser::WorldItem::Function(_) | wit_parser::WorldItem::Type { .. } => None,
            })
            .unwrap_or_else(|| panic!("`{name}` is exported"))
    };
    let take = func("take");
    let make = func("make");
    [
        render(&resolve, take.params[0].ty),
        render(&resolve, make.result.expect("`make` returns a value")),
    ]
}

fn render(resolve: &wit_parser::Resolve, ty: wit_parser::Type) -> String {
    let wit_parser::Type::Id(id) = ty else {
        return format!("{ty:?}");
    };
    let def = &resolve.types[id];
    match (&def.name, &def.kind) {
        (Some(name), _) => name.clone(),
        (None, wit_parser::TypeDefKind::Option(inner)) => {
            format!("option<{}>", render(resolve, *inner))
        }
        (None, kind) => format!("{kind:?}"),
    }
}

#[test]
fn aliased_signature_names_the_struct_record() {
    let aliased = signature_type_names(&compile(ALIASED));
    assert_eq!(aliased, signature_type_names(&compile(UNALIASED)));
    assert_eq!(aliased, ["foo", "foo"]);
}

/// A generic head read under an alias is the declaration it reaches too.
#[test]
fn aliased_generic_head_names_the_option() {
    const SOURCE: &str = r#"
use { Foo as u32 } from "./sub/alias_user_struct_as_primitive_dep.wado";
use { Option as Maybe } from "core:prelude";

export fn take(x: Maybe<u32>) -> i32 {
    if let Some(f) = x {
        return f.v;
    }
    return 0;
}

export fn make(v: i32) -> Maybe<u32> {
    return Some(u32 { v });
}
"#;
    assert_eq!(
        signature_type_names(&compile(SOURCE)),
        ["option<foo>", "option<foo>"]
    );
}

#[test]
fn aliased_signature_round_trips_the_record() {
    let engine = engine();
    let component = Component::new(engine, compile(ALIASED)).expect("component loads");
    runtime().block_on(async {
        let linker = linker(engine).expect("build linker");
        let mut store = Store::new(engine, WasiState::new());
        limit_store(&mut store, DEFAULT_TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        let make = lib_func(&mut store, &instance, LIB_WORLD_FQ, "make");
        let mut made = vec![Val::Bool(false)];
        make.call_async(&mut store, &[Val::S32(7)], &mut made)
            .await
            .expect("call `make`");
        assert_eq!(made[0], Val::Record(vec![("v".to_string(), Val::S32(7))]));

        let take = lib_func(&mut store, &instance, LIB_WORLD_FQ, "take");
        let mut taken = vec![Val::Bool(false)];
        take.call_async(&mut store, &made, &mut taken)
            .await
            .expect("call `take`");
        assert_eq!(taken[0], Val::S32(7));
    });
}
