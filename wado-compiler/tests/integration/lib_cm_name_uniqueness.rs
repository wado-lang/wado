//! Two library exports whose CM names differ only by hyphens are one name to
//! the Component Model (`Explainer.md` § "Name Uniqueness"), so the compiler
//! reports the pair rather than emitting a component the validator rejects.

use std::path::Path;

use wado_compiler::CompilerOptions;

use crate::common::compile_source_with_compiler_options;

const SOURCE: &str = r#"
export fn a_b() -> i32 {
    return 1;
}

export fn ab() -> i32 {
    return 2;
}
"#;

#[test]
fn exports_equal_once_hyphens_are_dropped_collide() {
    let options = CompilerOptions {
        lib_world: Some("test:unique/unique@0.1.0".to_string()),
        ..Default::default()
    };
    let err = compile_source_with_compiler_options(Path::new("lib.wado"), SOURCE, options)
        .expect_err("`a-b` and `ab` are one CM name");
    let message = err.to_string();
    assert!(
        message.contains("export `ab` becomes `ab`") && message.contains("function `a_b`"),
        "expected the export collision, got {message}"
    );
}
