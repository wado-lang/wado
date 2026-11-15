//! The `wado-lang:web` extern handles `package-web` generates from its vendored
//! `WebIDL` snapshot. See `docs/wep-2026-11-14-extern-handles.md`.

use crate::common::{WEB_PACKAGE, compile_against_web};

/// A program importing `names` whose `body` runs with `el`, a fresh `Element`,
/// in scope.
fn on_an_element(names: &str, body: &str) -> String {
    format!(
        "use {{ {names} }} from \"{WEB_PACKAGE}\";\n\
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

/// The main module's import of an operation `interface` declares.
fn web_import(interface: &str) -> String {
    format!("(import \"extern\" \"$wado-lang:web/{interface}")
}

/// A web import is a core import of the main module, its handle an
/// `externref`, so the component imports nothing of the platform.
#[test]
fn a_web_import_is_a_core_import_taking_an_externref() {
    let wat = compile_to_wat(&on_an_element("Dom", "el.set_id(\"app\");"));
    let set_id = wat
        .lines()
        .find(|line| line.contains(&web_import("element")))
        .unwrap_or_else(|| panic!("the element operation should be imported: {wat}"));
    assert!(
        set_id.contains("(param (ref extern)"),
        "the receiver should cross as an `externref`: {set_id}"
    );
    assert!(
        !wat.contains("(import \"wado-lang:web/"),
        "the component should import nothing of the platform: {wat}"
    );
    assert!(
        !wat.contains("(resource"),
        "an extern handle declares no CM resource type: {wat}"
    );
}

/// An inherited method is imported from the interface of the resource that
/// declares it, with the receiver passed through unchanged.
#[test]
fn an_inherited_method_calls_the_declaring_interface() {
    let wat = compile_to_wat(&on_an_element("Dom", INHERITED));
    assert!(
        wat.contains(&web_import("node")),
        "the declaring interface should be imported: {wat}"
    );
}

/// Both types are the same handle at the boundary, so an upcast converts
/// nothing: the two programs compile to the same component.
#[test]
fn an_upcast_is_a_no_op() {
    assert_eq!(
        compile_to_wat(&on_an_element("Dom, Node", UPCAST)),
        compile_to_wat(&on_an_element("Dom", INHERITED))
    );
}

/// An optional handle result is the declared `Option<Element>`, whose `None`
/// is the null reference.
#[test]
fn an_optional_extern_handle_result_lifts_to_the_declared_option() {
    let wat = compile_to_wat(&on_an_element(
        "Dom",
        "assert doc.get_element_by_id(\"app\") matches { None };\n\
         assert el.parent_node() matches { None };",
    ));
    assert!(
        wat.contains(&web_import("document")) && wat.contains(&web_import("node")),
        "both interfaces should be imported: {wat}"
    );
}

/// An optional handle parameter takes the caller's `Option<Node>`, whether the
/// argument is `null`, a `Some` literal, or a value of that type.
#[test]
fn an_optional_extern_handle_argument_keeps_the_declared_option() {
    let wat = compile_to_wat(&on_an_element(
        "Dom, Node",
        "let node: Node = el;\n\
         assert !el.contains(null);\n\
         assert el.is_same_node(Option::Some(node));\n\
         let other: Option<Node> = Option::Some(node);\n\
         assert el.is_equal_node(other);",
    ));
    assert!(wat.contains(&web_import("node")), "{wat}");
}

/// Compile `source`, holding it to its warnings as well: a program this slice
/// accepts has to be clean, not merely compilable.
fn compile_to_wat(source: &str) -> String {
    let compiled = compile_against_web(source);
    let warnings: Vec<&str> = compiled
        .warnings
        .iter()
        .map(|w| w.message.as_str())
        .collect();
    assert_eq!(warnings, Vec::<&str>::new());
    let result = compiled
        .result
        .unwrap_or_else(|e| panic!("expected the wado-lang:web slice to compile, got {e}"));
    wasmprinter::print_bytes(&result.wasm).expect("the component should print")
}
