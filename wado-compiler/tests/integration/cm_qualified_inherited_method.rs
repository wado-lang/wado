//! A resource operation's `#[cm(...)]` import is read off the signature the
//! call resolved to, so the qualified spelling of an inherited *instance*
//! method keeps it. Read from a receiver-less index instead, the binding was
//! lost and the call reached WIR naming a function no module holds.

use crate::common::compile_source;

const QUALIFIED_INHERITED_METHOD: &str = r#"
#[cm("wasi:demo/events@0.1.0#event-target", linearity = "unrestricted")]
resource EventTarget {
    #[cm("wasi:demo/events@0.1.0#tag")]
    #[cm_params("self")]
    fn tag(&self) -> String;
}

#[cm("wasi:demo/events@0.1.0#node", linearity = "unrestricted")]
resource Node extends EventTarget {}

export fn run() with EventTarget {
    let n = 1 as Node;
    let _ = EventTarget::tag(&n);
}
"#;

#[test]
fn a_qualified_inherited_method_keeps_its_cm_binding() {
    let result = compile_source(QUALIFIED_INHERITED_METHOD)
        .unwrap_or_else(|e| panic!("a qualified inherited method must compile: {e}"));
    let wat = wasmprinter::print_bytes(&result.wasm).expect("disassemble the component to WAT");

    assert!(
        wat.lines().map(str::trim).any(|line| {
            line.starts_with("(alias export $wasi:demo/events") && line.contains("\"tag\"")
        }),
        "the call must reach the import its `#[cm]` names; got\n{wat}"
    );
}
