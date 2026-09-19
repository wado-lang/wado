//! A `future<T>` payload is lifted as the record its own module declares. A user
//! record whose name a bundled `wasi:cli` type also spells used to be lifted as
//! that type instead — an enum, so a discriminant `i32` where a struct
//! reference belongs, and the core module failed validation (issue #2090).

use wado_compiler::OptLevel;

use crate::common::compile_lib_world;

const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

/// `ErrorCode` also names a `wasi:cli/types` enum. It is the only name that
/// interface spells as an enum, so a per-kind search by name finds exactly this
/// one and no ambiguity check declines it.
const SOURCE: &str = r#"
pub struct ErrorCode {
    a: i64,
    b: i64,
}

export fn sum_of(rx: Future<ErrorCode>) -> i64 {
    if let Some(v) = rx.read() {
        return v.a + v.b;
    }
    return -1;
}
"#;

#[test]
fn future_payload_is_lifted_as_its_own_record() {
    // Compilation validates the core module, so reaching the WAT at all is the
    // assertion; the ICE was a validation failure inside this helper.
    let wasm = compile_lib_world(SOURCE, LIB_WORLD_FQ, OptLevel::O0, None);
    let wat = wasmprinter::print_bytes(&wasm).expect("print the component");
    assert!(
        wat.contains("$cm_future_read_lib.wado/ErrorCode"),
        "the record payload's own future-read helper is what the export calls"
    );
}
