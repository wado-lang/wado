//! A `--lib` build's world is synthesized from its exports and imports what the
//! library uses, so an `#[ambient]` body runs there: a library whose only
//! stderr write is an assertion's message still imports `wasi:cli/stderr`.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::Component;

use crate::common::{compile_source_with_compiler_options, engine};

const LIB_WORLD_FQ: &str = "test:ambient/ambient@0.1.0";

#[test]
fn a_library_keeps_its_assertion_message() {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let source = r"
export fn positive(x: i32) -> i32 {
    assert x > 0;
    return x;
}
";
    let wasm = compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .expect("the library compiles")
        .wasm;
    let engine = engine();
    let component = Component::new(engine, &wasm).expect("the library is a component");
    let imports: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name.to_string())
        .collect();
    assert!(
        imports.iter().any(|name| name == "wasi:cli/stderr@0.3.0"),
        "imports: {imports:?}"
    );
}
