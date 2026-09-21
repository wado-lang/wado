//! An attribute is checked against the schema in `wado_compiler::attribute`:
//! an unknown name, a misplaced one, and a malformed argument list are rejected
//! where they are written.

use crate::common::compile_source;
use wado_compiler::CompileError;

fn error_message(source: &str) -> String {
    match compile_source(source).expect_err("expected the attribute to be rejected") {
        CompileError::Analyzer { message, .. } => message,
        other => panic!("expected an analyzer error, got: {other}"),
    }
}

#[test]
fn unknown_attribute_is_rejected() {
    let message = error_message(
        r#"
#[expect_trab]
test "typo" {
    assert 1 == 1;
}
"#,
    );
    assert!(
        message.contains("unknown attribute") && message.contains("expect_trab"),
        "unexpected message: {message}"
    );
}

#[test]
fn attribute_on_the_wrong_declaration_is_rejected() {
    let message = error_message(
        r#"
#[expect_trap]
struct Point { x: i32 }
"#,
    );
    assert!(
        message.contains("expect_trap") && message.contains("struct"),
        "unexpected message: {message}"
    );
}

#[test]
fn inner_only_attribute_written_as_an_outer_one_is_rejected() {
    let message = error_message(
        r#"
#[no_prelude]
fn main() {}
"#,
    );
    assert!(
        message.contains("no_prelude"),
        "unexpected message: {message}"
    );
}

#[test]
fn argument_shape_the_schema_fixes_is_rejected() {
    let message = error_message(
        r#"
#[timeout_ms("5000")]
test "string timeout" {
    assert 1 == 1;
}
"#,
    );
    assert!(
        message.contains("timeout_ms") && message.contains("one number"),
        "unexpected message: {message}"
    );
}

#[test]
fn a_well_formed_attribute_compiles() {
    let source = r#"
#[timeout_ms(5000)]
test "fine" {
    assert 1 == 1;
}

#[allow(dead_code)]
fn helper() -> i32 {
    return 1;
}

fn main() {
    let _ = helper();
}
"#;
    assert!(compile_source(source).is_ok());
}
