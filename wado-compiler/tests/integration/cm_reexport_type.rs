//! Re-exporting a named type owned by a CM component dependency: a `--lib`
//! whose export signature references a type imported from another component
//! (e.g. `Point` from `sub/cm-catalog.wasm`) must resolve that type to the
//! dependency's interface, not panic ("unresolved CM named type reference").
//! Regression for the CM instance emitter's missing component-interface
//! fallback (it had `wasi:` / `core:kiln/` fallbacks only).

use crate::common::compile_source_with_compiler_options;
use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

const LIB_WORLD_FQ: &str = "test:reexp/reexp@0.1.0";

fn compile_reexport() -> Vec<u8> {
    // `./sub/cm-catalog.wasm` resolves relative to this fixtures path.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/reexp.wado");
    let source = r#"
use { CmCatalog, Point } from "./sub/cm-catalog.wasm" with { type: "wasm" };
export fn mk(v: Point) -> Point {
    return CmCatalog::id_record(v);
}
"#;
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("re-exporting a component-owned type compiles (no unresolved-type ICE)")
        .wasm
}

#[test]
fn reexported_component_type_compiles_and_decodes() {
    let wasm = compile_reexport();
    // Was an ICE; now compiles and re-parses as a valid component whose export
    // signature references the (component-owned) point record.
    let decoded = wit_component::decode(&wasm).expect("decode composed component");
    let wit_component::DecodedWasm::Component(resolve, world) = decoded else {
        panic!("expected a component");
    };
    let mk = resolve.worlds[world]
        .exports
        .iter()
        .find_map(|(_, item)| match item {
            wit_parser::WorldItem::Interface { id, .. } => {
                resolve.interfaces[*id].functions.get("mk")
            }
            _ => None,
        })
        .expect("exported interface exposes `mk`");
    let wit_parser::Type::Id(param_id) = mk.params[0].ty else {
        panic!("mk param is not a named type");
    };
    assert_eq!(
        resolve.types[param_id].name.as_deref(),
        Some("point"),
        "mk's param is the point record"
    );
    // A malformed structural duplication would fail validation; `compile_reexport`
    // compiles with validation on, and the decode above re-parses the type.
}

/// The types a library publishes in its interface.
fn published_types(source: &str) -> Vec<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/adopt.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let wasm = compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("a library publishing an adopted type compiles")
        .wasm;
    let wit_component::DecodedWasm::Component(resolve, world) =
        wit_component::decode(&wasm).expect("decode the library component")
    else {
        panic!("expected a component");
    };
    resolve.worlds[world]
        .exports
        .iter()
        .flat_map(|(_, item)| match item {
            wit_parser::WorldItem::Interface { id, .. } => {
                resolve.interfaces[*id].types.keys().cloned().collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

/// A stdlib type that `export use` adopts is published in the library's own
/// interface, under the name the `export use` gives it, as a type the library
/// declares would be.
#[test]
fn adopted_stdlib_type_is_published_by_the_library() {
    let published = published_types(
        "export use { DirEntry, FileType as Kind } from \"core:fs\";\n\
         export fn kind_of(e: DirEntry) -> Kind { return e.type; }",
    );
    assert_eq!(published, ["kind", "dir-entry"]);
}

/// A type of the library's own submodule is published under the name its
/// `export use` gives it, and under no other.
#[test]
fn own_type_is_published_under_its_alias() {
    let published = published_types(
        "export use { Helper as Counter } from \"./sub/alias_shadows_same_name_a.wado\";\n\
         export fn count(c: Counter) -> i32 { return c.h; }",
    );
    assert_eq!(published, ["counter"]);
}

/// A newtype of the library's own submodule crosses as the alias it publishes.
#[test]
fn own_newtype_is_published_under_its_alias() {
    let published = published_types(
        "export use { Meters as Distance } from \"./sub/adopted_newtype.wado\";\n\
         export fn double(d: Distance) -> Distance { return d + d; }",
    );
    assert_eq!(published, ["distance"]);
}
