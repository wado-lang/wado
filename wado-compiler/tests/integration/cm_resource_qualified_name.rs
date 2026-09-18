//! A `[method]T.m` / `[static]T.m` / `[constructor]T` CM name applies to the
//! Component Model resource `T`, so the interface it names must declare one. An
//! unrestricted resource crosses the boundary as a plain handle and declares no
//! resource type, so such a name names nothing — which the component validator
//! rejected as an internal compiler error instead of the compiler reporting it.

use crate::common::compile_source;

/// A resource binding `[method]event-target.tag`, declared under the
/// `#[cm(...)]` the caller gives it.
fn binding(resource_attr: &str) -> String {
    format!(
        r#"
{resource_attr}
resource Subject {{
    #[cm("wasi:demo/events@0.1.0#[method]event-target.tag")]
    #[cm_params("self")]
    fn tag(&self) -> String;
}}

export fn run() {{}}
"#
    )
}

#[test]
fn a_method_name_on_an_unrestricted_resource_is_a_diagnostic() {
    let source =
        binding(r#"#[cm("wasi:demo/events@0.1.0#event-target", linearity = "unrestricted")]"#);
    let message = compile_source(&source)
        .expect_err("a method name whose receiver is erased must be a diagnostic")
        .to_string();
    assert!(
        message.contains("[method]event-target.tag") && message.contains("unrestricted"),
        "expected the erased-receiver error, got {message}"
    );
}

#[test]
fn a_method_name_whose_receiver_no_interface_declares_is_a_diagnostic() {
    let source = binding(r#"#[cm("wasi:demo/events@0.1.0#node")]"#);
    let message = compile_source(&source)
        .expect_err("a method name with no resource behind it must be a diagnostic")
        .to_string();
    assert!(
        message.contains("[method]event-target.tag") && message.contains("event-target"),
        "expected the undeclared-receiver error, got {message}"
    );
}

#[test]
fn a_method_name_on_a_declared_resource_is_accepted() {
    let source = binding(r#"#[cm("wasi:demo/events@0.1.0#event-target")]"#);
    compile_source(&source)
        .unwrap_or_else(|e| panic!("a method of a declared CM resource must compile: {e}"));
}
