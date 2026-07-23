//! `wado compile --lib` must lower a guest effect left unhandled at the library
//! boundary to a Component Model **import** of a synthesized interface, so a
//! consumer can satisfy it. An effect handled inside the library is
//! self-contained and imports nothing. `wit_component::decode` recovers the
//! component's world so the import surface is asserted structurally.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};
use wit_parser::{InterfaceId, Resolve, WorldItem};

const LIB_WORLD_FQ: &str = "spike:hltest/hltest@0.0.1";
const GUEST_IFACE_FQ: &str = "spike:hltest/highlight@0.0.1";

fn compile_lib_source(source: &str) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("hltest.wado"), source, options)
        .expect("compile inline --lib source")
        .wasm
}

/// The `ns:pkg/name@ver` FQ of an interface, as `wit-component` records it.
fn iface_fq(resolve: &Resolve, id: InterfaceId) -> Option<String> {
    let iface = &resolve.interfaces[id];
    let name = iface.name.clone()?;
    let pkg = &resolve.packages[iface.package?].name;
    Some(match &pkg.version {
        Some(v) => format!("{}:{}/{}@{}", pkg.namespace, pkg.name, name, v),
        None => format!("{}:{}/{}", pkg.namespace, pkg.name, name),
    })
}

/// The imported interface with FQ `fq`, if the component's world imports it.
fn imported_interface(wasm: &[u8], fq: &str) -> Option<(Resolve, InterfaceId)> {
    let decoded = wit_component::decode(wasm).expect("decode component");
    let (resolve, world) = match decoded {
        wit_component::DecodedWasm::Component(resolve, world) => (resolve, world),
        wit_component::DecodedWasm::WitPackage(..) => panic!("expected a component"),
    };
    let id = resolve.worlds[world]
        .imports
        .iter()
        .find_map(|(_, item)| match item {
            WorldItem::Interface { id, .. } if iface_fq(&resolve, *id).as_deref() == Some(fq) => {
                Some(*id)
            }
            _ => None,
        })?;
    Some((resolve, id))
}

#[test]
fn unhandled_guest_effect_becomes_a_cm_import() {
    let source = r#"
interface Highlight {
    fn highlight(code: String, lang: String) -> String;
}
export fn wrap(code: String, lang: String) -> String with Highlight {
    return Highlight::highlight(code, lang);
}
test "shape compiles" {}
"#;
    let wasm = compile_lib_source(source);
    let (resolve, id) = imported_interface(&wasm, GUEST_IFACE_FQ)
        .unwrap_or_else(|| panic!("guest effect not imported as `{GUEST_IFACE_FQ}`"));

    let func = resolve.interfaces[id]
        .functions
        .get("highlight")
        .expect("imported interface exposes `highlight`");
    assert_eq!(func.params.len(), 2, "highlight takes (code, lang)");
    assert!(
        matches!(func.result, Some(wit_parser::Type::String)),
        "highlight returns string, got {:?}",
        func.result
    );
}

#[test]
fn guest_effect_named_like_the_package_is_rejected() {
    // `Hltest` kebabs to `hltest`, the package's own interface segment, so the
    // minted import FQ would equal the library's export interface FQ. Rejected
    // with a rename hint rather than silently minting a colliding interface.
    let source = r#"
interface Hltest {
    fn op(code: String) -> String;
}
export fn wrap(code: String) -> String with Hltest {
    return Hltest::op(code);
}
test "shape" {}
"#;
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let err =
        common::compile_source_with_compiler_options(Path::new("hltest.wado"), source, options)
            .expect_err("a guest effect colliding with the package interface name is rejected");
    assert!(
        format!("{err}").contains("collides"),
        "expected a collision diagnostic, got: {err}"
    );
}

#[test]
fn internally_handled_effect_imports_nothing() {
    let source = r#"
interface Highlight {
    fn highlight(code: String, lang: String) -> String;
}
struct DefaultHl {}
impl Highlight for DefaultHl {
    fn highlight(&mut self, code: String, lang: String) -> String {
        resume `${lang}:${code}`;
    }
    ..trap
}
export fn render_highlighted(code: String, lang: String) -> String {
    let mut h = DefaultHl {};
    return with &mut h do { Highlight::highlight(code, lang) };
}
test "shape compiles" {}
"#;
    let wasm = compile_lib_source(source);
    assert!(
        imported_interface(&wasm, GUEST_IFACE_FQ).is_none(),
        "a fully-handled effect must not surface as a CM import"
    );
}
