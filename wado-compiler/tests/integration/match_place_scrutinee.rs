//! `match *r` reads the referent in place.
//!
//! Pattern lowering hoisted every non-`Local` scrutinee into a temp the fold
//! then defends, so `match *rule` deep-copied the whole variant to read one
//! field while `match rule` copied nothing.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

const SOURCE: &str = r#"
struct Simple { name: String, ids: List<i32> }
struct Multi { name: String, alts: List<String> }
variant Rule { S(Simple), M(Multi) }

#[inline(never)]
fn walk(rule: &Rule) -> i32 {
    match *rule {
        Rule::S(s) => { return s.ids.len(); },
        Rule::M(m) => { return m.alts.len(); },
    }
}

export fn run() {
    let rules: List<Rule> = [Rule::S(Simple { name: "a", ids: [1, 2] })];
    let mut n = 0;
    for let r of &rules { n += walk(r); }
    assert n == 2;
}
"#;

fn walk_body() -> String {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        retain_wir: true,
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("match_place_scrutinee_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wir_package = result.wir_package.as_ref().expect("wir retained");
    let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package);

    let start = wir_text
        .find("fn \"match_place_scrutinee_test.wado/walk\"")
        .expect("walk function in WIR");
    let rest = &wir_text[start..];
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn match_on_a_shared_deref_does_not_copy_the_referent() {
    let body = walk_body();
    assert!(
        !body.contains("array_copy") && !body.contains("array_new"),
        "reading through `*rule` must allocate nothing:\n{body}"
    );
    assert!(
        !body.contains("struct.new"),
        "no arm rebuilds the variant:\n{body}"
    );
}
