//! A closure costs its captures their move, not the whole frame's.
//!
//! The move analysis used to abandon any function that built one, so a single
//! `map(|x| …)` left every other local in that body copying.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

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

fn build_body() -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("closure_frame_moves_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"closure_frame_moves_test.wado/build\"")
        .expect("build function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn a_closure_leaves_the_rest_of_the_frame_moving() {
    let body = build_body();
    assert!(
        !body.contains("array_copy"),
        "the element the iterator already copied must move into `next`:\n{body}"
    );
}
