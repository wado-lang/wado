//! A `#[cm(...)]` member a user module declares. The interface it names decides
//! what happens: one the stdlib does not bundle is the module's own binding and
//! lowers to that import, while one the stdlib already declares into is refused,
//! since a CM interface has a single declaring module.

use crate::common::compile_source;

/// `wasi:demo/types` is nobody else's, so this module declares it.
const OWN_INTERFACE: &str = r#"
#[cm("wasi:demo/types@0.1.0#handle")]
resource Handle {
    #[cm("wasi:demo/types@0.1.0#[constructor]handle")]
    fn new() -> Handle;
}

export fn run() with Handle {
    let h = Handle::new();
}
"#;

/// `wasi:http/types` is the stdlib's, and `MyFields` would collide with what it
/// declares there.
const STDLIB_INTERFACE: &str = r#"
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
fn a_user_declared_member_of_its_own_interface_lowers_to_that_import() {
    compile_source(OWN_INTERFACE)
        .unwrap_or_else(|e| panic!("a module's own CM interface must compile: {e}"));
}

#[test]
fn declaring_into_a_stdlib_owned_interface_is_rejected() {
    let err = compile_source(STDLIB_INTERFACE)
        .err()
        .expect("declaring into a stdlib-owned interface must be a diagnostic");
    let message = err.to_string();
    assert!(
        message.contains("wasi:http/types@0.3.0") && message.contains("already declares"),
        "expected the single-declaring-module error, got {message}"
    );
}

/// The declaration alone lowers nothing, so it stays accepted — that is the
/// shape `extends` is checked in until the `web:*` modules are bundled.
#[test]
fn declaring_one_without_calling_it_still_compiles() {
    let source = r#"
#[cm("wasi:demo/types@0.1.0#handle")]
resource Handle {
    #[cm("wasi:demo/types@0.1.0#[constructor]handle")]
    fn new() -> Handle;
}

export fn run() {}
"#;
    assert!(
        compile_source(source).is_ok(),
        "an uncalled binding declaration lowers nothing"
    );
}
