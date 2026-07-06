//! `wado dump --tir-resolved` must unparse a generic struct declaration
//! without panicking.
//!
//! Regression: the resolved-stage snapshot shared its `Rc<RefCell<TypeTable>>`
//! with the downstream pipeline, so DCE's type-table `retain` (which drops a
//! generic decl's field `TypeParam`, unreachable from any concrete type)
//! punched holes the snapshot still referenced. Unparsing `struct Holder<T>`'s
//! `payload: T` field then panicked with "`TypeId`(..) not found in `TypeTable`".

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world, unparse::unparse_tir};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

const SOURCE: &str = r#"
struct Holder<T> {
    payload: T,
}

export fn run() {
    let h = Holder { payload: 1 };
}
"#;

#[test]
fn tir_resolved_unparses_generic_struct_without_panicking() {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
    ))
    .expect("dump succeeds");

    let modules = dump
        .tir_modules
        .expect("resolved TIR modules present after dump");

    // Unparsing every resolved module must not panic on the generic decl's
    // `TypeParam` field type, and the entry module must render `Holder<T>`.
    let mut saw_holder = false;
    for module in modules.values() {
        let text = unparse_tir(module);
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
