//! `web:` is a bundled namespace and `web:dom` the extern-handle slice Tide's
//! generator emits from the vendored `WebIDL` snapshot. See
//! `docs/wep-2026-04-01-tide.md`.

use crate::common::{check_diagnostics, compile_source};

/// A program whose `body` runs with `el`, a fresh `Element`, in scope.
fn on_an_element(body: &str) -> String {
    format!(
        "use {{ Dom, Node }} from \"web:dom\";\n\
         export fn run() with Dom {{\n\
             let doc = Dom::document();\n\
             let el = doc.create_element(\"div\", null);\n\
             {body}\n\
         }}\n"
    )
}

/// `set_text_content` is declared on `Node`, which `Element` extends.
const INHERITED: &str = "el.set_text_content(Option::Some(\"Hello, Wado!\"));";

/// The same call, reached through an upcast to the declaring resource.
const UPCAST: &str =
    "let parent: Node = el;\nparent.set_text_content(Option::Some(\"Hello, Wado!\"));";

/// An extern-handle-backed resource is an opaque `u32` at the CM boundary, not
/// a CM `resource`, so the component's imports carry no handle type.
#[test]
fn an_extern_handle_crosses_as_a_bare_u32() {
    let wat = compile_to_wat(&on_an_element("el.set_id(\"app\");"));
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
    let wat = compile_to_wat(&on_an_element(INHERITED));
    assert!(
        wat.contains("web:dom/node"),
        "the declaring interface should be imported: {wat}"
    );
}

/// Both types are the same handle at the boundary, so an upcast converts
/// nothing: the two programs compile to the same component.
#[test]
fn an_upcast_is_a_no_op() {
    assert_eq!(
        compile_to_wat(&on_an_element(UPCAST)),
        compile_to_wat(&on_an_element(INHERITED))
    );
}

/// An `option<extern-handle>` result lifts into the declared `Option<Element>`,
/// not the `Option<u32>` the boundary sees.
#[test]
fn an_optional_extern_handle_result_lifts_to_the_declared_option() {
    let wat = compile_to_wat(&on_an_element(
        "assert doc.get_element_by_id(\"app\") matches { None };\n\
         assert el.parent_node() matches { None };",
    ));
    assert!(
        wat.contains("web:dom/document") && wat.contains("web:dom/node"),
        "both interfaces should be imported: {wat}"
    );
}

/// An `option<extern-handle>` parameter is the caller's `Option<Node>` on both
/// sides of the binding, whether the argument is `null`, a `Some` literal, or a
/// value of that type.
#[test]
fn an_optional_extern_handle_argument_keeps_the_declared_option() {
    let wat = compile_to_wat(&on_an_element(
        "let node: Node = el;\n\
         assert !el.contains(null);\n\
         assert el.is_same_node(Option::Some(node));\n\
         let other: Option<Node> = Option::Some(node);\n\
         assert el.is_equal_node(other);",
    ));
    assert!(wat.contains("web:dom/node"), "{wat}");
}

/// Compile `source`, holding it to what `wado check` reports as well: a program
/// this slice accepts has to be clean, not merely compilable.
fn compile_to_wat(source: &str) -> String {
    assert_eq!(check_diagnostics(source), Vec::<String>::new());
    let result = compile_source(source)
        .unwrap_or_else(|e| panic!("expected the web:dom slice to compile, got {e}"));
    wasmprinter::print_bytes(&result.wasm).expect("the component should print")
}
