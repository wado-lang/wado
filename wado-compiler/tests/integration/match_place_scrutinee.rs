//! `match *r` reads the referent in place, as `match r` does.
//!
//! The temp pattern lowering minted for a non-`Local` scrutinee is one the fold
//! defends, so `match *rule` deep-copied the whole variant to read one field.

use std::path::Path;

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
    // No closing quote: `sroa_param` may have left the surviving definition
    // under a `$scalar` clone name, which does not change what is asserted here.
    crate::common::wir_function_body(
        Path::new("match_place_scrutinee_test.wado"),
        SOURCE,
        "fn \"match_place_scrutinee_test.wado/walk",
    )
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
