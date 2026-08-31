//! A by-reference `for` over a `List` of a boxable element allocates nothing
//! per iteration.
//!
//! `&variant` lowers to `Box<T>`, and the box reads the very index the next
//! statement bumps — adjacency cannot move its initializer to the use.

use std::path::Path;

use wado_compiler::OptLevel;

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
    crate::common::wir_function_body(
        Path::new("box_local_unwrap_test.wado"),
        SOURCE,
        OptLevel::O2,
        "fn \"box_local_unwrap_test.wado/count\"",
    )
}

#[test]
fn by_reference_iteration_boxes_no_element() {
    let body = count_body();
    assert!(
        !body.contains("struct.new"),
        "the loop must allocate nothing per element:\n{body}"
    );
}
