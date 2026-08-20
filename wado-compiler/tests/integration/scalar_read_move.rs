//! A scalar read out of a binding is not a use that costs it its move.
//!
//! `if it.tag == -1 { … } else { out.push(it) }` read `it.tag` with `it` live in
//! the other arm, and the whole-value read was then no longer a final use — so
//! the element the iterator had already copied was copied a second time.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

const SOURCE: &str = r#"
struct Item { ids: List<i32>, tag: i32 }

#[inline(never)]
fn make(k: i32) -> List<Item> { return [Item { ids: [k], tag: k }]; }

#[inline(never)]
fn keep(n: i32) -> List<Item> {
    let mut out: List<Item> = [];
    let mut seen: List<i32> = [];
    for let it of make(n) {
        if it.tag == -1 {
            if !seen.contains(&it.tag) { seen.push(it.tag); }
        } else {
            out.push(it);
        }
    }
    return out;
}

export fn run() {
    assert keep(0).len() == 1;
}
"#;

fn keep_body() -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("scalar_read_move_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"scalar_read_move_test.wado/keep\"")
        .expect("keep function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn a_scalar_read_leaves_the_whole_value_moving() {
    let body = keep_body();
    assert!(
        !body.contains("array_copy"),
        "the element the iterator already copied must move into `out`:\n{body}"
    );
}
