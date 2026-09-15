//! `wado dump --tir-resolved` must unparse a generic struct declaration
//! without panicking.
//!
//! Regression: the resolved-stage snapshot shared its `Rc<RefCell<TypeTable>>`
//! with the downstream pipeline, so DCE's type-table `retain` (which drops a
//! generic decl's field `TypeParam`, unreachable from any concrete type)
//! punched holes the snapshot still referenced. Unparsing `struct Holder<T>`'s
//! `payload: T` field then panicked with "`TypeId`(..) not found in `TypeTable`".

use crate::common::{InMemoryHost, block_on};
use wado_compiler::{OptLevel, dump_with_host_and_world, unparse::unparse_tir};

const SOURCE: &str = r#"
struct Holder<T> {
    payload: T,
}

global GREETING: String = "hello";
global COUNT: i32 = 42;

export fn run() {
    let h = Holder { payload: 1 };
    assert GREETING.len() == 5;
    assert COUNT == 42;
}
"#;

/// The resolved TIR of every module `SOURCE` pulls in, unparsed.
fn resolved_modules() -> Vec<String> {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        wado_compiler::OptOverrides::default(),
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dump succeeds");
    dump.tir_modules
        .expect("resolved TIR modules present after dump")
        .values()
        .map(unparse_tir)
        .collect()
}

#[test]
fn tir_resolved_unparses_generic_struct_without_panicking() {
    let modules = resolved_modules();

    // Unparsing every resolved module must not panic on the generic decl's
    // `TypeParam` field type, and the entry module must render `Holder<T>`.
    let mut saw_holder = false;
    for text in &modules {
        if text.contains("struct Holder") {
            saw_holder = true;
            assert!(
                text.contains("payload: T"),
                "generic struct field type should render as `T`, got:\n{text}"
            );
        }
    }
    assert!(saw_holder, "entry module should contain `struct Holder`");
}

/// A deferred global's slot holds a placeholder and its declared value is in
/// `$init$<NAME>`, so printing the slot bare reads as the value. `nir_unparse`
/// already says "deferred"; the TIR dump has to as well.
#[test]
fn tir_resolved_marks_a_deferred_global_rather_than_printing_its_placeholder() {
    let entry = resolved_modules()
        .into_iter()
        .find(|text| text.contains("global GREETING"))
        .expect("entry module declares GREETING");

    assert!(
        entry.contains("global GREETING: String = deferred "),
        "a deferred global must be marked, got:\n{entry}"
    );
    assert!(
        entry.contains("global COUNT: i32 = 42;"),
        "a direct global still prints its value, got:\n{entry}"
    );
}
