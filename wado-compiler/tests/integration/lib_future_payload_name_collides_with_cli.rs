//! A `future<T>` payload is lifted as the record its own module declares. A user
//! record whose name a bundled `wasi:cli` type also spells used to be lifted as
//! that type instead — an enum, so a discriminant `i32` where a struct
//! reference belongs, and the core module failed validation.

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
    let wasm = compile_lib_world(SOURCE, LIB_WORLD_FQ, OptLevel::O0, None);
    let decoded = wit_component::decode(&wasm).expect("decode the library world");
    let resolve = decoded.resolve();
    let record = resolve
        .types
        .iter()
        .find_map(|(_, t)| match &t.kind {
            wit_parser::TypeDefKind::Record(r) if t.name.as_deref() == Some("error-code") => {
                Some(r)
            }
            _ => None,
        })
        .expect("`error-code` at the boundary is this library's record, not the `wasi:cli` enum");
    let fields: Vec<(&str, wit_parser::Type)> = record
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.ty))
        .collect();
    assert_eq!(
        fields,
        vec![("a", wit_parser::Type::S64), ("b", wit_parser::Type::S64)],
        "the payload carries the fields the library declared"
    );
}
