//! A scalar read out of a binding is not a use that costs it its move.
//!
//! `if it.tag == -1 { … } else { out.push(it) }` made the whole-value read
//! non-final, so the element the iterator had already copied was copied twice.

use std::path::Path;

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

#[test]
fn a_scalar_read_leaves_the_whole_value_moving() {
    let body = crate::common::wir_function_body(
        Path::new("scalar_read_move_test.wado"),
        SOURCE,
        wado_compiler::OptLevel::O2,
        "fn \"scalar_read_move_test.wado/keep\"",
    );
    crate::common::assert_pushes_by_move(&body, "out");
}
