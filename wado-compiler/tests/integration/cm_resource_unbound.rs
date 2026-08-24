//! A call to a `#[cm(...)]` member a user module declares has no import behind
//! it. Reported as a diagnostic; it used to panic the WIR build.

const UNKNOWN_INTERFACE: &str = r#"
#[cm("wasi:demo/types@0.1.0#handle")]
resource Handle {
    #[cm("wasi:demo/types@0.1.0#[constructor]handle")]
    fn new() -> Handle;
}

export fn run() with Handle {
    let h = Handle::new();
}
"#;

/// The registry knows the interface but not this resource, so the constructor
/// still resolves to no import.
const KNOWN_INTERFACE: &str = r#"
#[cm("wasi:http/types@0.3.0#fields")]
resource MyFields {
    #[cm("wasi:http/types@0.3.0#[constructor]fields")]
    fn new() -> MyFields;
}

export fn run() with MyFields {
    let f = MyFields::new();
}
"#;

#[test]
fn calling_a_user_declared_cm_member_is_rejected() {
    for source in [UNKNOWN_INTERFACE, KNOWN_INTERFACE] {
        let err = crate::common::compile_source(source)
            .err()
            .unwrap_or_else(|| panic!("expected a diagnostic for {source}"));
        let message = err.to_string();
        assert!(
            message.contains("no bundled interface declares"),
            "expected the unbound-binding error, got {message}"
        );
    }
}

/// The declaration alone lowers nothing, so it stays accepted — that is the
/// shape `extends` is checked in until the `web:*` modules are bundled.
#[test]
fn declaring_one_without_calling_it_still_compiles() {
    let source = "#[cm(\"wasi:demo/types@0.1.0#handle\")]\n\
         resource Handle {\n    #[cm(\"wasi:demo/types@0.1.0#[constructor]handle\")]\n    fn new() -> Handle;\n}\n\
         export fn run() {}\n";
    assert!(
        crate::common::compile_source(source).is_ok(),
        "an uncalled binding declaration lowers nothing"
    );
}
