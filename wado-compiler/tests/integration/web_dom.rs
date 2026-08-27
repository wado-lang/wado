//! `web:` is a bundled namespace, on the same footing as `wasi:`, and
//! `web:dom` is the extern-ref binding slice Tide's generator will replace.
//! See `docs/wep-2026-04-01-tide.md`.

use crate::common::check_diagnostics;

const CREATE_AND_LABEL: &str = r#"
use { Dom } from "web:dom";

export fn run() with Dom {
    let doc = Dom::document();
    let el = doc.create_element("div");
    el.set_id("app");
    el.set_text_content("Hello, Wado!");
}
"#;

#[test]
fn a_web_dom_program_type_checks() {
    assert_eq!(check_diagnostics(CREATE_AND_LABEL), Vec::<String>::new());
}

/// `set_text_content` is declared on `Node`, two levels above `Document`'s
/// sibling `Element` — the call resolves through the chain.
#[test]
fn an_inherited_method_resolves_on_a_bundled_resource() {
    let source = r#"
use { Dom, Node } from "web:dom";

export fn run() with Dom {
    let doc = Dom::document();
    let el = doc.create_element("div");
    let parent: Node = el;
    parent.set_text_content("Hello");
}
"#;
    assert_eq!(check_diagnostics(source), Vec::<String>::new());
}
