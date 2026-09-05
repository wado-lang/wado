//! The `WebIDL` frontend, over a hand-written webidl2 snapshot.

use wado_from_idl::WadoCodeGenerator;
use wado_from_idl::webidl::{Snapshot, WebIdlOutput, transform};

/// The definitions of a snapshot, as the JSON text `snapshot.mjs` writes.
#[derive(Default)]
struct Definitions {
    interfaces: Vec<String>,
    mixins: Vec<String>,
    includes: Vec<String>,
}

impl Definitions {
    fn build(&self) -> Snapshot {
        let json = format!(
            r#"{{
  "webref": "3.83.1",
  "package": "dom",
  "slice": ["EventTarget", "Node", "Element", "Window"],
  "interfaces": [{}],
  "mixins": [{}],
  "includes": [{}],
  "typedefs": [{}]
}}"#,
            self.interfaces.join(", "),
            self.mixins.join(", "),
            self.includes.join(", "),
            typedef("Timestamp", &plain("double")),
        );
        serde_json::from_str(&json).expect("the test snapshot should parse")
    }

    fn generate(&self) -> (String, Vec<String>) {
        let WebIdlOutput { module, skipped } =
            transform(&self.build()).expect("the slice should transform");
        (WadoCodeGenerator::new().generate(&module), skipped)
    }
}

fn named(name: &str, nullable: bool) -> String {
    format!(
        r#"{{"type": "attribute-type", "extAttrs": [], "generic": "", "nullable": {nullable}, "union": false, "idlType": "{name}"}}"#
    )
}

fn plain(name: &str) -> String {
    named(name, false)
}

fn nullable(name: &str) -> String {
    named(name, true)
}

fn typedef(name: &str, ty: &str) -> String {
    format!(r#"{{"type": "typedef", "name": "{name}", "idlType": {ty}, "extAttrs": []}}"#)
}

fn attribute(name: &str, ty: &str, readonly: bool) -> String {
    format!(
        r#"{{"type": "attribute", "name": "{name}", "idlType": {ty}, "extAttrs": [], "special": "", "readonly": {readonly}}}"#
    )
}

fn argument(name: &str, ty: &str, optional: bool, default: &str) -> String {
    format!(
        r#"{{"type": "argument", "name": "{name}", "extAttrs": [], "idlType": {ty}, "default": {default}, "optional": {optional}, "variadic": false}}"#
    )
}

fn variadic(name: &str, ty: &str) -> String {
    argument(name, ty, true, "null").replace(r#""variadic": false"#, r#""variadic": true"#)
}

fn operation(name: &str, ret: &str, args: &[String], special: &str) -> String {
    format!(
        r#"{{"type": "operation", "name": "{name}", "idlType": {ret}, "arguments": [{}], "extAttrs": [], "special": "{special}"}}"#,
        args.join(", ")
    )
}

fn definition(
    kind: &str,
    name: &str,
    inheritance: &str,
    ext_attrs: &str,
    partial: bool,
    members: &[String],
) -> String {
    format!(
        r#"{{"type": "{kind}", "name": "{name}", "inheritance": {inheritance}, "partial": {partial}, "extAttrs": [{ext_attrs}], "members": [{}]}}"#,
        members.join(", ")
    )
}

fn interface(name: &str, inheritance: &str, ext_attrs: &str, members: &[String]) -> String {
    definition("interface", name, inheritance, ext_attrs, false, members)
}

fn partial(name: &str, members: &[String]) -> String {
    definition("interface", name, "null", "", true, members)
}

const GLOBAL: &str =
    r#"{"type": "extended-attribute", "name": "Global", "rhs": null, "arguments": []}"#;

/// `EventTarget ← Node ← Element`, and `Window`, the global.
fn chain() -> Definitions {
    let event_target = interface(
        "EventTarget",
        "null",
        "",
        &[
            r#"{"type": "constructor", "arguments": [], "extAttrs": []}"#.to_string(),
            operation(
                "dispatchEvent",
                &plain("boolean"),
                &[argument("event", &plain("Node"), false, "null")],
                "",
            ),
        ],
    );
    let node = interface(
        "Node",
        r#""EventTarget""#,
        "",
        &[
            attribute("nodeType", &plain("unsigned short"), true),
            attribute("textContent", &nullable("DOMString"), false),
            operation(
                "cloneNode",
                &plain("Node"),
                &[argument(
                    "subtree",
                    &plain("boolean"),
                    true,
                    r#"{"type": "boolean", "value": false}"#,
                )],
                "",
            ),
            r#"{"type": "const", "name": "ELEMENT_NODE", "idlType": {"type": "const-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "unsigned short"}, "extAttrs": [], "value": {"type": "number", "value": "1"}}"#.to_string(),
        ],
    );
    let element = interface(
        "Element",
        r#""Node""#,
        "",
        &[
            attribute("id", &plain("DOMString"), false),
            operation(
                "setAttribute",
                &plain("undefined"),
                &[
                    argument("qualifiedName", &plain("DOMString"), false, "null"),
                    argument("value", &plain("DOMString"), false, "null"),
                ],
                "",
            ),
            operation(
                "setSelectionRange",
                &plain("undefined"),
                &[
                    argument("start", &plain("unsigned long"), false, "null"),
                    argument("direction", &plain("DOMString"), true, "null"),
                ],
                "",
            ),
        ],
    );
    let window = interface(
        "Window",
        r#""EventTarget""#,
        GLOBAL,
        &[
            attribute("document", &plain("Node"), true),
            attribute("self", &plain("Window"), true),
            attribute("name", &plain("DOMString"), false),
            attribute("type", &plain("DOMString"), false),
            attribute("started", &plain("Timestamp"), true),
        ],
    );
    Definitions {
        interfaces: vec![event_target, node, element, window],
        ..Definitions::default()
    }
}

#[test]
fn an_interface_is_an_extern_handle_resource_with_its_own_cm_interface() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "#[cm(\"web:dom/event-target\", type = \"extern-handle\")]\npub resource EventTarget {"
        ),
        "{code}"
    );
    assert!(
        code.contains(
            "#[cm(\"web:dom/node\", type = \"extern-handle\")]\npub resource Node extends EventTarget {"
        ),
        "{code}"
    );
    assert!(
        code.contains("pub resource Element extends Node {"),
        "{code}"
    );
}

#[test]
fn an_attribute_is_a_getter_and_a_setter_and_readonly_drops_the_setter() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "    #[cm(\"web:dom/node#text-content\")]\n    #[cm_params(\"self\")]\n    fn text_content(&self) -> Option<String>;"
        ),
        "{code}"
    );
    assert!(
        code.contains(
            "    #[cm(\"web:dom/node#set-text-content\")]\n    #[cm_params(\"self\", \"value\")]\n    fn set_text_content(&self, value: Option<String>);"
        ),
        "{code}"
    );
    assert!(code.contains("fn node_type(&self) -> u16;"), "{code}");
    assert!(!code.contains("set_node_type"), "{code}");
}

#[test]
fn a_constructor_is_new_and_a_method_names_its_arguments_in_kebab_case() {
    let (code, _) = chain().generate();
    assert!(
        code.contains("    #[cm(\"web:dom/event-target#new\")]\n    fn new() -> EventTarget;"),
        "{code}"
    );
    assert!(
        code.contains(
            "    #[cm(\"web:dom/element#set-attribute\")]\n    #[cm_params(\"self\", \"qualified-name\", \"value\")]\n    fn set_attribute(&self, qualified_name: String, value: String);"
        ),
        "{code}"
    );
    assert!(
        code.contains("fn dispatch_event(&self, event: Node) -> bool;"),
        "{code}"
    );
}

#[test]
fn an_optional_argument_is_an_option() {
    let (code, _) = chain().generate();
    assert!(
        code.contains("fn clone_node(&self, subtree: Option<bool>) -> Node;"),
        "{code}"
    );
    assert!(
        code.contains("fn set_selection_range(&self, start: u32, direction: Option<String>);"),
        "{code}"
    );
}

#[test]
fn a_keyword_is_escaped_a_typedef_resolves_and_a_const_is_not_a_member() {
    let (code, _) = chain().generate();
    assert!(code.contains("fn self_(&self) -> Window;"), "{code}");
    // `type` is a keyword the parser takes as a name.
    assert!(code.contains("fn type(&self) -> String;"), "{code}");
    assert!(
        code.contains("fn set_type(&self, value: String);"),
        "{code}"
    );
    assert!(code.contains("fn started(&self) -> f64;"), "{code}");
    assert!(!code.contains("ELEMENT_NODE"), "{code}");
}

#[test]
fn the_global_yields_the_dom_effect_with_its_resource_typed_attributes() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "#[cm(\"web:dom/global\")]\npub interface Dom {\n    #[cm(\"web:dom/global#window\")]\n    fn window() -> Window;\n    #[cm(\"web:dom/global#document\")]\n    fn document() -> Node;\n}"
        ),
        "{code}"
    );
}

#[test]
fn a_member_the_slice_cannot_express_is_skipped_and_reported() {
    let promise = r#"{"type": "return-type", "extAttrs": [], "generic": "Promise", "nullable": false, "union": false, "idlType": [{"type": "return-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "undefined"}]}"#;
    let union = r#"{"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": true, "idlType": [{"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "DOMString"}, {"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "Node"}]}"#;
    let members = [
        attribute("attributes", &plain("NamedNodeMap"), true),
        operation("requestFullscreen", promise, &[], ""),
        operation(
            "append",
            &plain("undefined"),
            &[argument("nodes", union, false, "null")],
            "",
        ),
        // A trailing optional the slice cannot express leaves the rest intact.
        operation(
            "createElement",
            &plain("Element"),
            &[
                argument("localName", &plain("DOMString"), false, "null"),
                argument("options", union, true, r#"{"type": "dictionary"}"#),
            ],
            "",
        ),
        // A variadic has no default to fall back on, so the member goes.
        operation(
            "prepend",
            &plain("undefined"),
            &[variadic("nodes", &plain("Node"))],
            "",
        ),
        // Two overloads that both lower are ambiguous, so neither is emitted.
        operation("alert", &plain("undefined"), &[], ""),
        operation(
            "alert",
            &plain("undefined"),
            &[argument("message", &plain("DOMString"), false, "null")],
            "",
        ),
        // A special operation names no method.
        operation(
            "",
            &plain("Window"),
            &[argument("name", &plain("DOMString"), false, "null")],
            "getter",
        ),
    ];
    let mut defs = chain();
    defs.interfaces.push(partial("Element", &members));
    let (code, skipped) = defs.generate();
    assert!(
        code.contains("fn create_element(&self, local_name: String) -> Element;"),
        "{code}"
    );
    assert!(!code.contains("attributes"), "{code}");
    assert!(!code.contains("alert"), "{code}");
    assert_eq!(
        skipped,
        [
            "Element.attributes: `NamedNodeMap` is outside the slice",
            "Element.request_fullscreen: `Promise<…>`",
            "Element.append: `nodes`: union of 2 expressible types",
            "Element.prepend: `nodes`: variadic",
            "Element.alert: 2 overloads",
            "Element.(getter): getter operation",
        ]
    );
}

fn union(members: &[String], nullable: bool) -> String {
    format!(
        r#"{{"type": "attribute-type", "extAttrs": [], "generic": "", "nullable": {nullable}, "union": true, "idlType": [{}]}}"#,
        members.join(", ")
    )
}

#[test]
fn a_union_collapses_only_where_the_guest_supplies_the_value() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[
            attribute(
                "innerHTML",
                &union(&[plain("TrustedHTML"), plain("DOMString")], false),
                false,
            ),
            attribute(
                "event",
                &union(&[plain("Node"), plain("undefined")], false),
                true,
            ),
            attribute(
                "hidden",
                &union(&[plain("boolean"), plain("DOMString")], false),
                false,
            ),
            attribute(
                "script",
                &union(
                    &[plain("HTMLScriptElement"), plain("SVGScriptElement")],
                    false,
                ),
                true,
            ),
        ],
    ));
    let (code, skipped) = defs.generate();
    // A `TrustedHTML` the slice cannot build is nothing lost on the way in.
    assert!(
        code.contains("fn set_inner_html(&self, value: String);"),
        "{code}"
    );
    // `undefined` is the nullability marker, not a dropped constituent.
    assert!(code.contains("fn event(&self) -> Option<Node>;"), "{code}");
    assert_eq!(
        skipped,
        [
            "Element.inner_html: union narrowed in a result: `TrustedHTML` is outside the slice",
            "Element.hidden: union of 2 expressible types",
            "Element.script: union: `HTMLScriptElement` is outside the slice; `SVGScriptElement` is outside the slice",
        ]
    );
}

#[test]
fn an_html_constructor_is_not_new() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[r#"{"type": "constructor", "arguments": [], "extAttrs": [{"type": "extended-attribute", "name": "HTMLConstructor", "rhs": null, "arguments": []}]}"#.to_string()],
    ));
    let (code, skipped) = defs.generate();
    assert!(!code.contains("fn new() -> Element"), "{code}");
    assert_eq!(skipped, ["Element.new: HTMLConstructor"]);
}

#[test]
fn a_mixin_folds_into_each_including_interface() {
    let mut defs = chain();
    defs.mixins.push(definition(
        "interface mixin",
        "ParentNode",
        "null",
        "",
        false,
        &[operation(
            "querySelector",
            &nullable("Element"),
            &[argument("selectors", &plain("DOMString"), false, "null")],
            "",
        )],
    ));
    defs.includes.push(
        r#"{"type": "includes", "extAttrs": [], "target": "Element", "includes": "ParentNode"}"#
            .to_string(),
    );
    let (code, skipped) = defs.generate();
    assert!(
        code.contains(
            "    #[cm(\"web:dom/element#query-selector\")]\n    #[cm_params(\"self\", \"selectors\")]\n    fn query_selector(&self, selectors: String) -> Option<Element>;"
        ),
        "{code}"
    );
    assert_eq!(skipped, Vec::<String>::new());
}

#[test]
fn a_second_global_is_rejected() {
    let mut defs = chain();
    defs.interfaces[1] = interface("Node", r#""EventTarget""#, GLOBAL, &[]);
    let err = transform(&defs.build()).expect_err("two globals are an error");
    assert!(
        err.to_string().contains("one `[Global]` interface"),
        "{err}"
    );
}

#[test]
fn a_child_redeclaring_an_inherited_method_is_rejected() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[attribute("nodeType", &plain("unsigned short"), true)],
    ));
    let err = transform(&defs.build()).expect_err("an override is an error");
    assert!(
        err.to_string()
            .contains("`Element.node_type` redeclares a method inherited from `Node`"),
        "{err}"
    );
}

#[test]
fn a_parent_outside_the_slice_is_rejected() {
    let mut defs = Definitions::default();
    defs.interfaces
        .push(interface("Element", r#""CharacterData""#, "", &[]));
    for name in ["EventTarget", "Node", "Window"] {
        defs.interfaces.push(interface(name, "null", "", &[]));
    }
    let err = transform(&defs.build()).expect_err("an open chain is an error");
    assert!(
        err.to_string()
            .contains("`Element` extends `CharacterData`, which is not in the slice"),
        "{err}"
    );
}
