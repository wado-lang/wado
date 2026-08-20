//! A value-producing labeled block is a break target.
//!
//! Without its entry on the exit stack every `break` it holds resolved to "every
//! local live", so one sequence literal after a loop left the frame copying.

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
    let mut next_configs: List<Cfg> = [];
    let mut opaque_alts: List<i32> = [];
    let mut groups: List<List<i32>> = [];
    for let mut t = 0; t < tokens.len(); t += 1 {
        let tk = tokens[t];
        for let mut i = 0; i < closed.len(); i += 1 {
            if closed[i].pos > 0 {
                let advanced = advance(&closed[i], tk);
                for let a of advanced {
                    if a.pos == -1 {
                        if !opaque_alts.contains(&a.alt) {
                            opaque_alts.push(a.alt);
                        }
                    } else {
                        next_configs.push(a);
                    }
                }
            }
        }
        groups.push([tk]);
    }
    if groups.len() > 100 { next_configs = []; }
    return next_configs;
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
        Path::new("labeled_block_exits_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"labeled_block_exits_test.wado/build\"")
        .expect("build function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn a_sequence_literal_does_not_pin_the_frame() {
    let body = build_body();
    assert!(
        !body.contains("array_copy"),
        "the element the iterator already copied must move into `next`:\n{body}"
    );
}
