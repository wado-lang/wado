//! A by-reference `for` over a `List` of a boxable element allocates nothing
//! per iteration.
//!
//! `&variant` lowers to `Box<T>`, so the loop mints one per element. The
//! adjacency elider cannot reach it — the box reads the very index the next
//! statement bumps, so its initializer may not move to the use — while
//! `unwrap_box_locals` needs no motion: the local is retyped to the field it
//! wraps and the `struct.new` goes.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

const SOURCE: &str = r#"
struct Inner { name: String, ids: List<i32> }
variant Node { Leaf(Inner), Pair([Inner, Inner]) }

#[inline(never)]
fn count(nodes: &List<Node>) -> i32 {
    let mut n = 0;
    for let node of nodes {
        match node {
            Node::Leaf(i) => n += i.ids.len(),
            Node::Pair([a, b]) => n += a.ids.len() + b.ids.len(),
        }
    }
    return n;
}

export fn run() {
    let nodes: List<Node> = [Node::Leaf(Inner { name: "a", ids: [1, 2] })];
    assert count(&nodes) == 2;
}
"#;

fn count_body() -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("box_local_unwrap_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"box_local_unwrap_test.wado/count\"")
        .expect("count function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn by_reference_iteration_boxes_no_element() {
    let body = count_body();
    assert!(
        !body.contains("struct.new"),
        "the loop must allocate nothing per element:\n{body}"
    );
}
