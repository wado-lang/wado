//! A closure costs its captures their move, not the whole frame's.
//!
//! The move analysis used to abandon any function that built one, so a single
//! `map(|x| …)` left every other local in that body copying.

use std::path::Path;

const SOURCE: &str = r#"
struct Cfg { alt: i32, elems: List<i32>, pos: i32 }

#[inline(never)]
fn advance(c: &Cfg, tk: i32) -> List<Cfg> {
    return [Cfg { ..*c, pos: c.pos + tk }];
}

#[inline(never)]
fn build(closed: &List<Cfg>, tokens: &List<i32>) -> List<Cfg> {
    let bumped: List<Cfg> = closed.iter_ref().map(|c: &Cfg| Cfg { ..*c, pos: c.pos + 1 }).collect();
    let mut next: List<Cfg> = [];
    let mut opaque: List<i32> = [];
    for let mut t = 0; t < tokens.len(); t += 1 {
        for let mut i = 0; i < closed.len(); i += 1 {
            if bumped[i].pos > 0 {
                for let a of advance(&closed[i], tokens[t]) {
                    if a.pos == -1 {
                        if !opaque.contains(&a.alt) { opaque.push(a.alt); }
                    } else {
                        next.push(a);
                    }
                }
            }
        }
    }
    return next;
}

export fn run() {
    let closed: List<Cfg> = [Cfg { alt: 0, elems: [1], pos: 1 }];
    assert build(&closed, &[1]).len() == 1;
}
"#;

#[test]
fn a_closure_leaves_the_rest_of_the_frame_moving() {
    let body = crate::common::wir_function_body(
        Path::new("closure_frame_moves_test.wado"),
        SOURCE,
        wado_compiler::OptLevel::O2,
        "fn \"closure_frame_moves_test.wado/build\"",
    );
    crate::common::assert_pushes_by_move(&body, "next");
}
