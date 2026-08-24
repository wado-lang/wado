//! `const_object_globalization` must not hoist the array a `[]` literal builds
//! when the list built over it is then pushed into.
//!
//! `[]` lowers to `List<T>::from(array.new_fixed<T>())`, and `from` keeps the
//! array it is handed as the list's spine — not a copy, because the literal was
//! fresh and the ownership analysis elided one. Hoisting that array into a
//! module global therefore hands every call the same storage to `push` into.
//! `core:zlib`'s `build_huffman_tree` accumulated its `sym_list` across calls
//! that way and wrote a literal-tree symbol index into the 30-element
//! distance-tree lengths.
//!
//! `from` is small enough to inline at every stock level, which is what kept
//! the hole out of reach; a zero inline budget is the supported configuration
//! that leaves the call standing.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

const SOURCE: &str = r#"
fn grow(n: i32) -> i32 {
    let mut xs: List<i32> = [];
    for let mut i = 0; i < n; i += 1 {
        xs.push(i);
    }
    return xs.len();
}

export fn run() {
    // Two calls: the second must not see the first's pushes.
    assert grow(builtin::black_box(2)) == 2;
    assert grow(builtin::black_box(2)) == 2;
}
"#;

/// The WIR with nothing inlined, so `build` stays a call.
fn wir_without_inlining() -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        inline_threshold: Some(0),
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("const_global_builder_alias_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    wado_compiler::wir_unparse::unparse_wir(wir_package)
}

#[test]
fn literal_storage_is_not_hoisted_to_a_shared_global() {
    let wir = wir_without_inlining();
    let globals: Vec<&str> = wir
        .lines()
        .filter(|line| line.starts_with("global "))
        .filter(|line| line.contains("//List<i32>\"") || line.contains("Array<i32>"))
        .collect();
    assert!(
        globals.is_empty(),
        "the builder's literal was hoisted to a shared global:\n{}",
        globals.join("\n")
    );
}

#[test]
fn the_conversion_call_survives_so_the_alias_is_reachable() {
    // Guards the fixture: if `from` were inlined here, the shape above would
    // not exercise the alias and the first test would pass vacuously.
    let wir = wir_without_inlining();
    assert!(
        wir.contains("From<Array<T>>::from"),
        "`from` was inlined, so this test no longer exercises the alias"
    );
}
