//! The `WebIDL` frontend, over a hand-written webidl2 snapshot.

use wado_from_idl::WadoCodeGenerator;
use wado_from_idl::webidl::{
    Cause, Lost, Skip, Snapshot, WebIdlOutput, generate, skip_table, transform,
};

/// The definitions of a snapshot, as the JSON text `snapshot.mjs` writes.
#[derive(Default)]
struct Definitions {
    interfaces: Vec<String>,
    mixins: Vec<String>,
    includes: Vec<String>,
    callbacks: Vec<String>,
    dictionaries: Vec<String>,
    enums: Vec<String>,
    namespaces: Vec<String>,
}

impl Definitions {
    fn build(&self) -> Snapshot {
        let json = format!(
            r#"{{
  "webref": "3.83.1",
  "package": "wado-lang:web",
  "module": "dom",
  "slice": ["EventTarget", "Node", "Element", "Window"],
  "interfaces": [{}],
  "mixins": [{}],
  "includes": [{}],
  "typedefs": [{}],
  "callbacks": [{}],
  "dictionaries": [{}],
  "enums": [{}],
  "namespaces": [{}]
}}"#,
            self.interfaces.join(", "),
            self.mixins.join(", "),
            self.includes.join(", "),
            typedef("Timestamp", &plain("double")),
            self.callbacks.join(", "),
            self.dictionaries.join(", "),
            self.enums.join(", "),
            self.namespaces.join(", "),
        );
        serde_json::from_str(&json).expect("the test snapshot should parse")
    }

    fn transform(&self) -> (String, Vec<Skip>) {
        let WebIdlOutput {
            module, skipped, ..
        } = transform(&self.build()).expect("the slice should transform");
        (WadoCodeGenerator::new().generate(&module), skipped)
    }

    fn generate(&self) -> (String, Vec<String>) {
        let (code, skipped) = self.transform();
        (code, skipped.iter().map(Skip::to_string).collect())
    }
}

fn dictionary(name: &str, partial: bool) -> String {
    definition("dictionary", name, "null", "", partial, &[])
}

fn namespace(name: &str, members: &[String]) -> String {
    definition("namespace", name, "null", "", false, members)
}

fn const_member(name: &str) -> String {
    format!(
        r#"{{"type": "const", "name": "{name}", "idlType": {}, "extAttrs": [], "value": {{"type": "number", "value": "1"}}}}"#,
        plain("unsigned short")
    )
}

/// An `iterable`, `maplike`, `setlike` or `async_iterable` declaration.
fn iteration(kind: &str) -> String {
    format!(
        r#"{{"type": "{kind}", "idlType": [{}], "arguments": [], "extAttrs": [], "readonly": false, "async": false}}"#,
        plain("DOMString")
    )
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
    argument(name, ty, false, "null").replace(r#""variadic": false"#, r#""variadic": true"#)
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
fn an_interface_is_an_unrestricted_resource_with_its_own_cm_interface() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "#[cm(\"wado-lang:web/event-target\", linearity = \"unrestricted\", repr = \"externref\", classes = \"0..=3\")]\npub resource EventTarget {"
        ),
        "{code}"
    );
    assert!(
        code.contains(
            "#[cm(\"wado-lang:web/node\", linearity = \"unrestricted\", repr = \"externref\", classes = \"1..=2\")]\npub resource Node extends EventTarget {"
        ),
        "{code}"
    );
    assert!(
        code.contains("pub resource Element extends Node {"),
        "{code}"
    );
}

/// Each interface's classes come just before its descendants', so one range
/// covers a subtree.
#[test]
fn classes_number_the_inheritance_forest_in_pre_order() {
    let (code, _) = chain().generate();
    for (resource, classes) in [
        ("event-target", "0..=3"),
        ("node", "1..=2"),
        ("element", "2..=2"),
        ("window", "3..=3"),
    ] {
        assert!(
            code.contains(&format!(
                "#[cm(\"wado-lang:web/{resource}\", linearity = \"unrestricted\", repr = \"externref\", classes = \"{classes}\")]"
            )),
            "{resource} should carry {classes}: {code}"
        );
    }
}

#[test]
fn an_attribute_is_a_getter_and_a_setter_and_readonly_drops_the_setter() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "    #[cm(\"wado-lang:web/node#text-content\")]\n    #[cm_params(\"self\")]\n    fn text_content(&self) -> Option<String>;"
        ),
        "{code}"
    );
    assert!(
        code.contains(
            "    #[cm(\"wado-lang:web/node#set-text-content\")]\n    #[cm_params(\"self\", \"value\")]\n    fn set_text_content(&self, value: Option<String>);"
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
        code.contains(
            "    #[cm(\"wado-lang:web/event-target#new\")]\n    fn new() -> EventTarget;"
        ),
        "{code}"
    );
    assert!(
        code.contains(
            "    #[cm(\"wado-lang:web/element#set-attribute\")]\n    #[cm_params(\"self\", \"qualified-name\", \"value\")]\n    fn set_attribute(&self, qualified_name: String, value: String);"
        ),
        "{code}"
    );
    assert!(
        code.contains("fn dispatch_event(&self, event: Node) -> bool;"),
        "{code}"
    );
}

#[test]
fn an_optional_argument_takes_its_webidl_default_or_null() {
    let members = [
        operation(
            "stepUp",
            &plain("undefined"),
            &[argument(
                "n",
                &plain("long"),
                true,
                r#"{"type": "number", "value": "0xFF"}"#,
            )],
            "",
        ),
        // A number is read by WebIDL's grammar, where a leading `0` is octal
        // and a float may start or end at its `.`, and written in Wado's.
        operation(
            "setMode",
            &plain("undefined"),
            &[
                argument(
                    "mode",
                    &plain("long"),
                    true,
                    r#"{"type": "number", "value": "-0755"}"#,
                ),
                argument(
                    "ratio",
                    &plain("double"),
                    true,
                    r#"{"type": "number", "value": ".5"}"#,
                ),
                argument(
                    "scale",
                    &plain("double"),
                    true,
                    r#"{"type": "number", "value": "5."}"#,
                ),
            ],
            "",
        ),
        operation(
            "open",
            &plain("undefined"),
            &[argument(
                "target",
                &plain("DOMString"),
                true,
                r#"{"type": "string", "value": "_\"blank\\"}"#,
            )],
            "",
        ),
        // A default of another kind than the type is the browser's to apply.
        operation(
            "scroll",
            &plain("undefined"),
            &[argument(
                "smooth",
                &plain("boolean"),
                true,
                r#"{"type": "dictionary"}"#,
            )],
            "",
        ),
        // Only trailing parameters take a default in Wado.
        operation(
            "write",
            &plain("undefined"),
            &[
                argument("prefix", &plain("DOMString"), true, "null"),
                variadic("text", &plain("DOMString")),
            ],
            "",
        ),
    ];
    let mut defs = chain();
    defs.interfaces.push(partial("Element", &members));
    let (code, _) = defs.generate();
    assert!(
        code.contains("fn clone_node(&self, subtree: bool = false) -> Node;"),
        "{code}"
    );
    assert!(
        code.contains(
            "fn set_selection_range(&self, start: u32, direction: Option<String> = null);"
        ),
        "{code}"
    );
    assert!(code.contains("fn step_up(&self, n: i32 = 255);"), "{code}");
    assert!(
        code.contains("fn set_mode(&self, mode: i32 = -493, ratio: f64 = 0.5, scale: f64 = 5.0);"),
        "{code}"
    );
    assert!(
        code.contains(r#"fn open(&self, target: String = "_\"blank\\");"#),
        "{code}"
    );
    assert!(
        code.contains("fn scroll(&self, smooth: Option<bool> = null);"),
        "{code}"
    );
    assert!(
        code.contains("fn write(&self, prefix: Option<String>, text: List<String>);"),
        "{code}"
    );
}

#[test]
fn a_keyword_is_escaped_and_a_typedef_resolves() {
    let (code, _) = chain().generate();
    assert!(code.contains("fn self_(&self) -> Window;"), "{code}");
    // `type` is a keyword the parser takes as a name.
    assert!(code.contains("fn type(&self) -> String;"), "{code}");
    assert!(
        code.contains("fn set_type(&self, value: String);"),
        "{code}"
    );
    assert!(code.contains("fn started(&self) -> f64;"), "{code}");
}

#[test]
fn the_global_yields_the_dom_effect_with_its_resource_typed_attributes() {
    let (code, _) = chain().generate();
    assert!(
        code.contains(
            "#[cm(\"wado-lang:web/global\")]\npub interface Dom {\n    #[cm(\"wado-lang:web/global#window\")]\n    fn window() -> Window;\n    #[cm(\"wado-lang:web/global#document\")]\n    fn document() -> Node;\n}"
        ),
        "{code}"
    );
}

#[test]
fn a_member_the_slice_cannot_express_is_skipped_and_reported() {
    let union = r#"{"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": true, "idlType": [{"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "DOMString"}, {"type": "argument-type", "extAttrs": [], "generic": "", "nullable": false, "union": false, "idlType": "Node"}]}"#;
    let members = [
        attribute("attributes", &plain("NamedNodeMap"), true),
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
            &[variadic("nodes", &plain("NodeList"))],
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
            "Element.append: `nodes`: union of 2 expressible types",
            "Element.create_element: optional `options` dropped: union of 2 expressible types",
            "Element.prepend: `nodes`: `NodeList` is outside the slice",
            "Element.alert: 2 overloads",
            "Element.(getter): getter operation",
        ]
    );
}

#[test]
fn a_variadic_argument_is_a_list_the_glue_spreads() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[
            operation(
                "append",
                &plain("undefined"),
                &[variadic("nodes", &plain("Node"))],
                "",
            ),
            operation(
                "write",
                &plain("undefined"),
                &[
                    argument("prefix", &plain("DOMString"), false, "null"),
                    variadic("text", &plain("DOMString")),
                ],
                "",
            ),
        ],
    ));
    let (code, skipped) = defs.generate();
    assert!(
        code.contains("fn append(&self, nodes: List<Node>);"),
        "{code}"
    );
    assert!(
        code.contains("fn write(&self, prefix: String, text: List<String>);"),
        "{code}"
    );
    assert_eq!(skipped, Vec::<String>::new());

    let glue = generate(&defs.build(), "test.json")
        .expect("the slice should transform")
        .glue;
    assert!(
        glue.contains(
            "p0.write($string(p1, p2), ...$list(p3, p4, 8, (at) => \
             $string($view().getUint32(at, true), $view().getUint32(at + 4, true))));"
        ),
        "{glue}"
    );
    // A handle in a list is the index of its slot, which the host reads.
    assert!(
        glue.contains(
            "p0.append(...$list(p1, p2, 4, (at) => \
             $core.exports[$SLOT_GET]($view().getUint32(at, true))));"
        ),
        "{glue}"
    );
}

fn sequence(generic: &str, element: &str) -> String {
    format!(
        r#"{{"type": "return-type", "extAttrs": [], "generic": "{generic}", "nullable": false, "union": false, "idlType": [{element}]}}"#
    )
}

#[test]
fn a_sequence_is_a_list_either_way() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[
            operation("path", &sequence("sequence", &plain("Node")), &[], ""),
            operation("names", &sequence("sequence", &plain("DOMString")), &[], ""),
            operation(
                "observe",
                &plain("undefined"),
                &[argument(
                    "targets",
                    &sequence("sequence", &plain("Node")),
                    false,
                    "null",
                )],
                "",
            ),
            attribute("ports", &sequence("FrozenArray", &plain("long")), true),
            // A list's element is a scalar, a string or a handle.
            operation(
                "nested",
                &sequence("sequence", &sequence("sequence", &plain("long"))),
                &[],
                "",
            ),
        ],
    ));
    let (code, skipped) = defs.generate();
    assert!(code.contains("fn path(&self) -> List<Node>;"), "{code}");
    assert!(code.contains("fn names(&self) -> List<String>;"), "{code}");
    assert!(
        code.contains("fn observe(&self, targets: List<Node>);"),
        "{code}"
    );
    assert!(code.contains("fn ports(&self) -> List<i32>;"), "{code}");
    assert_eq!(
        skipped,
        ["Element.nested: `sequence` of a list, a nullable type or a callback"]
    );

    let glue = generate(&defs.build(), "test.json")
        .expect("the slice should transform")
        .glue;
    // The host puts each handle it answers in a slot of the guest's.
    assert!(
        glue.contains(
            "const $SLOT_GET = \"$cm_extern_slot_get\";\nconst $SLOT_PUT = \"$cm_extern_slot_put\";"
        ),
        "{glue}"
    );
    assert!(
        glue.contains(
            "$putList($ret, $value, 4, 4, (at, item) => {\n          \
             $view().setUint32(at, $core.exports[$SLOT_PUT](item), true);\n        });"
        ),
        "{glue}"
    );
    assert!(
        glue.contains(
            "$putList($ret, $value, 8, 4, (at, item) => {\n          \
             $putString(at, item);\n        });"
        ),
        "{glue}"
    );
    assert!(
        glue.contains(
            "p0.observe($list(p1, p2, 4, (at) => \
             $core.exports[$SLOT_GET]($view().getUint32(at, true))));"
        ),
        "{glue}"
    );
}

fn string_enum(name: &str, values: &[&str]) -> String {
    let values: Vec<String> = values
        .iter()
        .map(|value| format!(r#"{{"type": "enum-value", "value": "{value}"}}"#))
        .collect();
    format!(
        r#"{{"type": "enum", "name": "{name}", "values": [{}], "extAttrs": []}}"#,
        values.join(", ")
    )
}

#[test]
fn an_enum_is_a_wado_enum_crossing_as_its_cases_index() {
    let mut defs = chain();
    defs.enums = vec![
        string_enum("RequestMode", &["cors", "no-cors", "same_origin", "flipY"]),
        // Only an enum a bound member names is declared.
        string_enum("Unnamed", &["one"]),
    ];
    defs.interfaces.push(partial(
        "Element",
        &[
            attribute("mode", &plain("RequestMode"), false),
            attribute("fallback", &nullable("RequestMode"), true),
            operation(
                "modes",
                &sequence("sequence", &plain("RequestMode")),
                &[],
                "",
            ),
            operation(
                "request",
                &plain("undefined"),
                &[
                    argument(
                        "allowed",
                        &sequence("sequence", &plain("RequestMode")),
                        false,
                        "null",
                    ),
                    argument(
                        "mode",
                        &plain("RequestMode"),
                        true,
                        r#"{"type": "string", "value": "no-cors"}"#,
                    ),
                    argument("other", &plain("RequestMode"), true, "null"),
                ],
                "",
            ),
        ],
    ));
    let (code, skipped) = defs.generate();
    assert!(
        code.contains(
            "#[cm(\"wado-lang:web/types#request-mode\")]\n\
             pub enum RequestMode {\n    Cors,\n    NoCors,\n    SameOrigin,\n    FlipY,\n}\n"
        ),
        "{code}"
    );
    assert!(!code.contains("Unnamed"), "{code}");
    assert!(code.contains("fn mode(&self) -> RequestMode;"), "{code}");
    assert!(
        code.contains("fn set_mode(&self, value: RequestMode);"),
        "{code}"
    );
    assert!(
        code.contains("fn fallback(&self) -> Option<RequestMode>;"),
        "{code}"
    );
    assert!(
        code.contains("fn modes(&self) -> List<RequestMode>;"),
        "{code}"
    );
    assert!(
        code.contains(
            "fn request(&self, allowed: List<RequestMode>, \
             mode: RequestMode = RequestMode::NoCors, other: Option<RequestMode> = null);"
        ),
        "{code}"
    );
    assert_eq!(skipped, [] as [&str; 0]);

    let glue = generate(&defs.build(), "test.json")
        .expect("the slice should transform")
        .glue;
    assert!(
        glue.contains(
            "const $ENUMS = {\n  \
             RequestMode: [\"cors\", \"no-cors\", \"same_origin\", \"flipY\"],\n};\n"
        ),
        "{glue}"
    );
    // A case crosses as its index, and is one byte in memory.
    assert!(
        glue.contains("return $case($ENUMS.RequestMode, p0.mode);"),
        "{glue}"
    );
    assert!(glue.contains("p0.mode = $ENUMS.RequestMode[p1];"), "{glue}");
    assert!(
        glue.contains("$view().setUint8($ret + 1, $case($ENUMS.RequestMode, $value));"),
        "{glue}"
    );
    assert!(
        glue.contains(
            "$putList($ret, $value, 1, 1, (at, item) => {\n          \
             $view().setUint8(at, $case($ENUMS.RequestMode, item));\n        });"
        ),
        "{glue}"
    );
    assert!(
        glue.contains(
            "p0.request($list(p1, p2, 1, (at) => $ENUMS.RequestMode[$view().getUint8(at)]), \
             $ENUMS.RequestMode[p3], (p4 === 0 ? undefined : $ENUMS.RequestMode[p5]));"
        ),
        "{glue}"
    );
}

/// A value Wado cannot spell as a case leaves its enum, and each member
/// naming it, without a binding.
#[test]
fn an_enum_whose_values_name_no_cases_is_skipped() {
    let mut defs = chain();
    defs.enums = vec![
        string_enum("Dimension", &["2d", "3d"]),
        string_enum("Blocker", &["", "frame"]),
        string_enum("Flip", &["flip-y", "flipY"]),
    ];
    defs.interfaces.push(partial(
        "Element",
        &[
            attribute("dimension", &plain("Dimension"), true),
            attribute("blocker", &plain("Blocker"), true),
            attribute("flip", &plain("Flip"), true),
        ],
    ));
    let (code, skipped) = defs.transform();
    assert!(!code.contains("pub enum"), "{code}");
    let reported: Vec<(String, Lost, Cause)> = skipped
        .iter()
        .map(|s| (s.to_string(), s.lost, s.reason.cause.clone()))
        .collect();
    let expected = [
        ("Dimension: the value `2d` names no case", Lost::Definition),
        ("Blocker: the value `` names no case", Lost::Definition),
        (
            "Flip: the values `flip-y` and `flipY` both name the case `FlipY`",
            Lost::Definition,
        ),
        (
            "Element.dimension: `Dimension` is an enum without a binding",
            Lost::Member,
        ),
        (
            "Element.blocker: `Blocker` is an enum without a binding",
            Lost::Member,
        ),
        (
            "Element.flip: `Flip` is an enum without a binding",
            Lost::Member,
        ),
    ]
    .map(|(line, lost)| (line.to_string(), lost, Cause::Enum));
    assert_eq!(reported, expected);
}

#[test]
fn a_promise_is_an_async_call_settling_to_a_result() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[
            operation("ready", &sequence("Promise", &plain("Node")), &[], ""),
            operation("lock", &sequence("Promise", &plain("undefined")), &[], ""),
            operation("title", &sequence("Promise", &plain("DOMString")), &[], ""),
            // A promise is a member's whole result.
            operation(
                "all",
                &sequence("sequence", &sequence("Promise", &plain("Node"))),
                &[],
                "",
            ),
        ],
    ));
    let (code, skipped) = defs.generate();
    assert!(
        code.contains("async fn ready(&self) -> AsyncCall<Result<Node, Rejection>>;"),
        "{code}"
    );
    assert!(
        code.contains("async fn lock(&self) -> AsyncCall<Result<(), Rejection>>;"),
        "{code}"
    );
    assert!(
        code.contains(
            "#[cm(\"wado-lang:web/rejection\", linearity = \"unrestricted\", repr = \"externref\")]\n\
             pub resource Rejection {"
        ),
        "{code}"
    );
    assert!(code.contains("fn message(&self) -> String;"), "{code}");
    assert_eq!(skipped, ["Element.all: `sequence` of `Promise<…>`"]);

    let glue = generate(&defs.build(), "test.json")
        .expect("the slice should transform")
        .glue;
    // The import answers the promise; `wait` has the result written.
    assert!(
        glue.contains(
            "\"$wado-lang:web/element#ready\": (p0) => {\n        \
             return $settle(p0.ready(), 4, ($ret, $value) => {\n          \
             $view().setUint32($ret, $core.exports[$SLOT_PUT]($value), true);\n        \
             });\n      },"
        ),
        "{glue}"
    );
    assert!(
        glue.contains(
            "\"$wado-lang:web/element#lock\": (p0) => {\n        \
             return $settle(p0.lock(), 4, ($ret, $value) => {\n        });\n      },"
        ),
        "{glue}"
    );
    assert!(
        glue.contains("\"$promise-wait\": new WebAssembly.Suspending("),
        "{glue}"
    );
    assert!(
        glue.contains(
            "\"$wado-lang:web/rejection#message\": (p0, $ret) => {\n        \
             const $value = String(p0.message ?? p0);"
        ),
        "{glue}"
    );
}

fn callback(name: &str, ret: &str, args: &[String]) -> String {
    format!(
        r#"{{"type": "callback", "name": "{name}", "idlType": {ret}, "arguments": [{}], "extAttrs": []}}"#,
        args.join(", ")
    )
}

#[test]
fn a_callback_is_a_closure_the_host_calls_back_with_scalars_and_handles() {
    let listener = definition(
        "callback interface",
        "EventListener",
        "null",
        "",
        false,
        &[operation(
            "handleEvent",
            &plain("undefined"),
            &[argument("event", &plain("Node"), false, "null")],
            "",
        )],
    );
    let mut defs = chain();
    defs.callbacks = vec![
        listener,
        callback(
            "FrameRequestCallback",
            &plain("undefined"),
            &[argument("time", &plain("double"), false, "null")],
        ),
        callback(
            "Filter",
            &plain("boolean"),
            &[argument("node", &plain("Node"), false, "null")],
        ),
        callback(
            "TextCallback",
            &plain("undefined"),
            &[argument("text", &plain("DOMString"), false, "null")],
        ),
        callback(
            "MaybeCallback",
            &plain("undefined"),
            &[argument("count", &plain("long"), true, "null")],
        ),
    ];
    let members = [
        operation(
            "addEventListener",
            &plain("undefined"),
            &[
                argument("type", &plain("DOMString"), false, "null"),
                argument("callback", &nullable("EventListener"), false, "null"),
            ],
            "",
        ),
        operation(
            "requestAnimationFrame",
            &plain("unsigned long"),
            &[argument(
                "callback",
                &plain("FrameRequestCallback"),
                false,
                "null",
            )],
            "",
        ),
        operation(
            "filter",
            &plain("undefined"),
            &[argument("filter", &plain("Filter"), false, "null")],
            "",
        ),
        operation(
            "onText",
            &plain("undefined"),
            &[argument("callback", &plain("TextCallback"), false, "null")],
            "",
        ),
        operation(
            "onMaybe",
            &plain("undefined"),
            &[argument("callback", &plain("MaybeCallback"), false, "null")],
            "",
        ),
        operation("currentListener", &plain("EventListener"), &[], ""),
    ];
    defs.interfaces.push(partial("Element", &members));
    let (code, skipped) = defs.generate();
    assert!(code.contains("callback: fn mut(Node) with Dom);"), "{code}");
    assert!(
        code.contains("callback: fn mut(f64) with Dom) -> u32;"),
        "{code}"
    );
    assert_eq!(
        skipped,
        [
            "Element.filter: `filter`: a callback returning a value",
            "Element.on_text: `callback`: callback argument `text`: neither a scalar nor a handle",
            "Element.on_maybe: `callback`: callback argument `count`: optional",
            "Element.current_listener: a callback in a result",
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
            "    #[cm(\"wado-lang:web/element#query-selector\")]\n    #[cm_params(\"self\", \"selectors\")]\n    fn query_selector(&self, selectors: String) -> Option<Element>;"
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

/// A resource may not override, so the parent's method serves the child: what
/// `DOMPoint.x` asks for over `DOMPointReadOnly.x`, whose setter is its own.
#[test]
fn a_child_redeclaring_an_inherited_method_keeps_the_parents() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Element",
        &[attribute("textContent", &nullable("DOMString"), false)],
    ));
    defs.interfaces.push(partial(
        "Node",
        &[attribute("id", &plain("DOMString"), true)],
    ));
    let (code, skipped) = defs.transform();
    assert_eq!(code.matches("fn text_content(&self)").count(), 1, "{code}");
    assert_eq!(code.matches("fn id(&self)").count(), 1, "{code}");
    assert_eq!(code.matches("fn set_id(&self").count(), 1, "{code}");
    let lines: Vec<String> = skipped.iter().map(Skip::to_string).collect();
    assert_eq!(
        lines,
        [
            "Element.id: redeclares a method inherited from `Node`",
            "Element.text_content: redeclares a method inherited from `Node`",
            "Element.set_text_content: redeclares a method inherited from `Node`",
        ]
    );
    assert!(skipped.iter().all(|s| s.reason.cause == Cause::Override));
    let glue = generate(&defs.build(), "idl/dom.webidl.json")
        .expect("the slice should generate")
        .glue;
    assert!(glue.contains("web/node#id\""), "{glue}");
    assert!(!glue.contains("web/element#id\""), "{glue}");
}

/// What `SVGElement.className` is, an `SVGAnimatedString` over the `DOMString`
/// of `Element.className`: the parent's method would answer with the wrong
/// type, so the child's binds under its interface's name.
#[test]
fn a_child_redeclaring_with_another_type_binds_under_its_interfaces_name() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "EventTarget",
        &[
            attribute("label", &plain("DOMString"), false),
            operation("item", &plain("long"), &[], ""),
        ],
    ));
    defs.interfaces.push(partial(
        "Node",
        &[attribute("label", &plain("DOMString"), true)],
    ));
    let index = argument("index", &plain("long"), false, "null");
    defs.interfaces.push(partial(
        "Element",
        &[
            attribute("label", &plain("long"), false),
            operation("item", &plain("long"), &[index], ""),
        ],
    ));
    let (code, skipped) = defs.generate();
    assert_eq!(
        skipped,
        ["Node.label: redeclares a method inherited from `EventTarget`"]
    );
    for declaration in [
        "fn element_label(&self) -> i32;",
        "fn set_element_label(&self, value: i32);",
        "fn element_item(&self, index: i32) -> i32;",
        "#[unavailable(\"`Element` redeclares it with another type; use `element_label`\")]\n    fn label(&self);",
        "#[unavailable(\"`Element` redeclares it with another type; use `set_element_label`\")]\n    fn set_label(&self);",
        "#[unavailable(\"`Element` redeclares it with another type; use `element_item`\")]\n    fn item(&self);",
        "#[unavailable(\"`Node` redeclares `label` as read-only\")]\n    fn set_label(&self);",
    ] {
        assert!(code.contains(declaration), "{declaration}\n{code}");
    }
    let glue = generate(&defs.build(), "idl/dom.webidl.json")
        .expect("the slice should generate")
        .glue;
    assert!(glue.contains("web/element#set-label\""), "{glue}");
}

/// A grandchild takes a member back from every binding above it, the one its
/// parent bound under its own name included.
#[test]
fn a_redeclaration_hides_every_binding_above_it() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "EventTarget",
        &[
            attribute("label", &plain("DOMString"), false),
            operation("item", &plain("long"), &[], ""),
        ],
    ));
    let index = argument("index", &plain("long"), false, "null");
    defs.interfaces.push(partial(
        "Node",
        &[
            attribute("label", &plain("long"), false),
            operation("item", &plain("long"), &[index], ""),
        ],
    ));
    defs.interfaces.push(partial(
        "Element",
        &[
            attribute("label", &plain("long"), true),
            operation("item", &plain("DOMString"), &[], ""),
        ],
    ));
    let (code, _) = defs.generate();
    let element = code
        .split("pub resource Element")
        .nth(1)
        .expect("Element is bound");
    let element = &element[..element.find("\n}").expect("the resource closes")];
    for declaration in [
        "#[unavailable(\"`Element` redeclares `label` as read-only\")]\n    fn set_node_label(&self);",
        "#[unavailable(\"`Element` redeclares it with another type; use `element_item`\")]\n    fn node_item(&self);",
        "#[unavailable(\"`Element` redeclares it with another type; use `element_item`\")]\n    fn item(&self);",
    ] {
        assert!(element.contains(declaration), "{declaration}\n{code}");
    }
    assert!(!element.contains("fn node_label(&self);"), "{code}");
}

/// A child may make writable again what its parent redeclared read-only. The
/// parent's reservation stands between it and the setter above, so its own
/// binds under its interface's name.
#[test]
fn a_setter_restored_below_a_read_only_redeclaration_binds_under_its_interfaces_name() {
    let mut defs = chain();
    for (interface, readonly) in [("EventTarget", false), ("Node", true), ("Element", false)] {
        let label = attribute("label", &plain("DOMString"), readonly);
        defs.interfaces.push(partial(interface, &[label]));
    }
    let (code, _) = defs.generate();
    for declaration in [
        "fn set_element_label(&self, value: String);",
        "#[unavailable(\"`Element` restores the setter `Node` hides; use `set_element_label`\")]\n    fn set_label(&self);",
    ] {
        assert!(code.contains(declaration), "{declaration}\n{code}");
    }
}

/// What `MediaStreamTrack.clone` is to its children: the parent's method hands
/// back the child's object, which a type pattern narrows.
#[test]
fn a_child_narrowing_only_the_result_keeps_the_parents() {
    let mut defs = chain();
    for interface in ["EventTarget", "Node", "Element"] {
        let twin = operation("twin", &nullable(interface), &[], "");
        defs.interfaces.push(partial(interface, &[twin]));
    }
    let (code, skipped) = defs.generate();
    assert_eq!(code.matches("fn twin(&self)").count(), 1, "{code}");
    assert_eq!(
        skipped,
        [
            "Node.twin: redeclares a method inherited from `EventTarget`",
            "Element.twin: redeclares a method inherited from `Node`",
        ]
    );
}

/// What `SVGElement.className` does to `Element`'s setter. An attribute is one
/// accessor, so only the attribute's own setter goes, and it goes from a
/// resource left with nothing else to declare.
#[test]
fn a_read_only_redeclaration_hides_the_inherited_setter_alone() {
    let mut defs = chain();
    defs.interfaces.push(partial(
        "EventTarget",
        &[
            attribute("label", &plain("DOMString"), false),
            operation("setPosition", &plain("undefined"), &[], ""),
        ],
    ));
    assert!(defs.interfaces[1].contains(r#""name": "Node""#));
    defs.interfaces[1] = interface(
        "Node",
        r#""EventTarget""#,
        "",
        &[attribute("label", &plain("DOMString"), true)],
    );
    defs.interfaces.push(partial(
        "Element",
        &[attribute("position", &plain("long"), true)],
    ));
    let (code, _) = defs.generate();
    let node = "pub resource Node extends EventTarget {\n    #[unavailable(\"`Node` redeclares `label` as read-only\")]\n    fn set_label(&self);\n}";
    assert!(code.contains(node), "{code}");
    assert_eq!(code.matches("#[unavailable(").count(), 1, "{code}");
}

/// What `MouseEvent.initMouseEvent` asks for: the arguments a caller may leave
/// out go, and the member stays.
#[test]
fn trailing_optionals_go_until_the_parameters_cross_as_core_values() {
    let required = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o",
    ]
    .map(|name| argument(name, &plain("long"), false, "null"));
    let optional = ["p", "q"].map(|name| argument(name, &plain("long"), true, "null"));
    let mut defs = chain();
    defs.interfaces.push(partial(
        "Node",
        &[operation(
            "draw",
            &plain("undefined"),
            &[&required[..], &optional[..]].concat(),
            "",
        )],
    ));
    let (code, skipped) = defs.transform();
    assert!(code.contains("o: i32);"), "{code}");
    let reported: Vec<(String, Lost, Cause)> = skipped
        .iter()
        .map(|s| (s.to_string(), s.lost, s.reason.cause.clone()))
        .collect();
    let expected = [(
        "Node.draw: optional `p` dropped: parameters passed through memory".to_string(),
        Lost::Argument,
        Cause::ParamsInMemory,
    )];
    assert_eq!(reported, expected);
}

#[test]
fn every_member_kind_parses_and_one_without_a_binding_is_reported() {
    let members = [
        const_member("ELEMENT_NODE"),
        iteration("iterable"),
        iteration("maplike"),
        iteration("setlike"),
        iteration("async_iterable"),
        // A bare `stringifier;` has no return type.
        r#"{"type": "operation", "name": "", "arguments": [], "extAttrs": [], "special": "stringifier"}"#.to_string(),
        attribute("origin", &plain("DOMString"), true)
            .replace(r#""special": """#, r#""special": "static""#),
        // The receiver and sixteen numbers are one more than cross as core values.
        operation(
            "draw",
            &plain("undefined"),
            &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p"]
                .map(|name| argument(name, &plain("long"), false, "null")),
            "",
        ),
    ];
    let mut defs = chain();
    defs.interfaces.push(partial("Node", &members));
    let (code, skipped) = defs.transform();
    assert!(!code.contains("ELEMENT_NODE"), "{code}");
    let reported: Vec<(String, Cause)> = skipped
        .iter()
        .map(|s| {
            assert_eq!(s.lost, Lost::Member);
            (s.to_string(), s.reason.cause.clone())
        })
        .collect();
    let expected = [
        ("Node.ELEMENT_NODE: a constant", Cause::Constant),
        (
            "Node.(iterable): an iteration declaration",
            Cause::Iteration,
        ),
        ("Node.(maplike): an iteration declaration", Cause::Iteration),
        ("Node.(setlike): an iteration declaration", Cause::Iteration),
        (
            "Node.(async_iterable): an iteration declaration",
            Cause::Iteration,
        ),
        (
            "Node.(stringifier): stringifier operation",
            Cause::SpecialOperation,
        ),
        ("Node.origin: static attribute", Cause::StaticAttribute),
        (
            "Node.draw: parameters passed through memory",
            Cause::ParamsInMemory,
        ),
    ]
    .map(|(line, cause)| (line.to_string(), cause));
    assert_eq!(reported, expected);
}

#[test]
fn a_type_without_a_binding_names_what_it_is() {
    let typed = |ty: &str| attribute(&ty.to_lowercase(), &plain(ty), true);
    let members = [
        typed("EventInit"),
        typed("Mode"),
        typed("any"),
        typed("object"),
        typed("Uint8Array"),
        typed("bigint"),
        typed("WorkerOnly"),
        attribute("names", &sequence("record", &plain("DOMString")), true),
    ];
    let mut defs = chain();
    defs.interfaces.push(partial("Element", &members));
    defs.dictionaries = vec![
        dictionary("EventInit", false),
        dictionary("EventInit", true),
    ];
    defs.enums = vec![string_enum("Mode", &["2d"])];
    defs.namespaces = vec![namespace(
        "console",
        &[
            operation("log", &plain("undefined"), &[], ""),
            const_member("LEVEL"),
        ],
    )];
    let (_, skipped) = defs.transform();
    let reported: Vec<(String, Lost, Cause)> = skipped
        .iter()
        .map(|s| (s.to_string(), s.lost, s.reason.cause.clone()))
        .collect();
    let expected = [
        (
            "EventInit: a dictionary",
            Lost::Definition,
            Cause::Dictionary,
        ),
        (
            "Mode: the value `2d` names no case",
            Lost::Definition,
            Cause::Enum,
        ),
        (
            "console.log: a namespace member",
            Lost::Member,
            Cause::Namespace,
        ),
        (
            "console.LEVEL: a namespace member",
            Lost::Member,
            Cause::Namespace,
        ),
        (
            "Element.eventinit: `EventInit` is a dictionary",
            Lost::Member,
            Cause::Dictionary,
        ),
        (
            "Element.mode: `Mode` is an enum without a binding",
            Lost::Member,
            Cause::Enum,
        ),
        ("Element.any: `any`", Lost::Member, Cause::Any),
        ("Element.object: `object`", Lost::Member, Cause::Object),
        (
            "Element.uint8array: `Uint8Array` is a buffer",
            Lost::Member,
            Cause::Buffer,
        ),
        ("Element.bigint: `bigint`", Lost::Member, Cause::BigInt),
        (
            "Element.workeronly: `WorkerOnly` is outside the slice",
            Lost::Member,
            Cause::OutsideSlice,
        ),
        (
            "Element.names: `record<…>`",
            Lost::Member,
            Cause::Generic("record".to_string()),
        ),
    ]
    .map(|(line, lost, cause)| (line.to_string(), lost, cause));
    assert_eq!(reported, expected);
}

/// A wrapper rewords a reason and keeps its cause, so the table counts what
/// would have to lower for the member to.
#[test]
fn the_skip_table_counts_each_cause_by_what_was_lost() {
    let mut defs = chain();
    defs.dictionaries = vec![dictionary("EventInit", false)];
    defs.interfaces.push(partial(
        "Element",
        &[
            operation(
                "init",
                &plain("undefined"),
                &[argument(
                    "a",
                    &sequence("sequence", &plain("EventInit")),
                    false,
                    "null",
                )],
                "",
            ),
            operation(
                "fire",
                &plain("undefined"),
                &[argument("init", &plain("EventInit"), true, "null")],
                "",
            ),
            attribute("state", &plain("any"), true),
        ],
    ));
    let (_, skipped) = defs.transform();
    assert_eq!(
        skip_table(&skipped),
        "\
Cause         Definitions  Members  Overloads  Arguments
a dictionary            1        1          0          1
`any`                   0        1          0          0
Total                   1        2          0          1
"
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

#[test]
fn an_inheritance_cycle_is_rejected() {
    let mut defs = Definitions::default();
    defs.interfaces
        .push(interface("Node", r#""Element""#, "", &[]));
    defs.interfaces
        .push(interface("Element", r#""Node""#, "", &[]));
    for name in ["EventTarget", "Window"] {
        defs.interfaces.push(interface(name, "null", "", &[]));
    }
    let err = transform(&defs.build()).expect_err("a cycle is an error");
    assert!(
        err.to_string()
            .contains("`Node`, `Element` inherit from each other in a cycle"),
        "{err}"
    );
}
