//! `web:` is a bundled namespace, on the same footing as `wasi:`, and
//! `web:dom` is the extern-handle binding slice Tide's generator will replace.
//! See `docs/wep-2026-04-01-tide.md`.

use crate::common::{check_diagnostics, compile_source};

/// Every call here resolves on the receiver's own declaration.
const OWN_METHODS: &str = r#"
use { Dom } from "web:dom";

export fn run() with Dom {
    let doc = Dom::document();
    let el = doc.create_element("div");
    el.set_id("app");
}
"#;

/// `set_text_content` is declared on `Node`, which `Element` extends.
const INHERITED_METHOD: &str = r#"
use { Dom } from "web:dom";

export fn run() with Dom {
    let doc = Dom::document();
    let el = doc.create_element("div");
    el.set_text_content("Hello, Wado!");
}
"#;

/// The upcast to the ancestor and the call through it, both on the same handle.
const UPCAST: &str = r#"
use { Dom, Node } from "web:dom";

export fn run() with Dom {
    let doc = Dom::document();
    let el = doc.create_element("div");
    let parent: Node = el;
    parent.set_text_content("Hello, Wado!");
}
"#;

#[test]
fn a_web_dom_program_type_checks() {
    assert_eq!(check_diagnostics(OWN_METHODS), Vec::<String>::new());
}

#[test]
fn an_inherited_method_type_checks() {
    assert_eq!(check_diagnostics(INHERITED_METHOD), Vec::<String>::new());
}

/// An extern-handle-backed resource is an opaque `u32` at the CM boundary, not a
/// CM `resource`, so the component's imports carry no handle type.
#[test]
fn an_extern_handle_crosses_as_a_bare_u32() {
    let wat = compile_to_wat(OWN_METHODS);
    assert!(
        wat.contains("web:dom/element"),
        "the element interface should be imported: {wat}"
    );
    assert!(
        !wat.contains("(resource"),
        "an extern-handle resource declares no CM resource type: {wat}"
    );
}

/// An inherited method is imported from the interface of the resource that
/// declares it, with the receiver passed through unchanged.
#[test]
fn an_inherited_method_calls_the_declaring_interface() {
    let wat = compile_to_wat(INHERITED_METHOD);
    assert!(
        wat.contains("web:dom/node"),
        "the declaring interface should be imported: {wat}"
    );
}

/// Both types are the same handle at the boundary, so an upcast converts
/// nothing: the two programs compile to the same component.
#[test]
fn an_upcast_is_a_no_op() {
    assert_eq!(check_diagnostics(UPCAST), Vec::<String>::new());
    assert_eq!(compile_to_wat(UPCAST), compile_to_wat(INHERITED_METHOD));
}

fn compile_to_wat(source: &str) -> String {
    let result = compile_source(source)
        .unwrap_or_else(|e| panic!("expected the web:dom slice to compile, got {e}"));
    wasmprinter::print_bytes(&result.wasm).expect("the component should print")
}
