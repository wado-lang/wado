//! A fixed array literal read back only at constant indices must not allocate.
//!
//! `core:simd` builds one per vector literal — `From<Array<i64>> for u64x2`
//! passes it to `lane()` once per lane — so every `u64x2` literal over
//! non-constant elements paid for an `array.new_fixed` plus a bounds-checked
//! read per lane.

use std::path::Path;

use crate::common::wir_function_body;
use wado_compiler::OptLevel;

const SOURCE: &str = r#"
use { u64x2 } from "core:simd";

#[inline(never)]
fn lanes(a: i64, b: i64) -> u64 {
    let v: u64x2 = [a, b];
    return v.extract_lane(0) as u64 ^ v.extract_lane(1) as u64;
}

export fn run() {
    assert lanes(builtin::black_box(1), builtin::black_box(2)) == 3;
}
"#;

const FILE: &str = "array_literal_sroa_test.wado";

fn lanes_body(opt_level: OptLevel) -> String {
    wir_function_body(
        Path::new(FILE),
        SOURCE,
        opt_level,
        &format!("fn \"{FILE}/lanes\""),
    )
}

fn assert_no_array(opt_level: OptLevel) {
    let body = lanes_body(opt_level);
    for spelling in ["array.new_fixed", "array_len", "array_get_value"] {
        assert!(
            !body.contains(spelling),
            "a vector literal's lanes must reach `i64x2.replace_lane` without \
             an array, found `{spelling}`:\n{body}"
        );
    }
}

#[test]
fn vector_literal_lanes_allocate_no_array() {
    assert_no_array(OptLevel::O2);
}

#[test]
fn vector_literal_lanes_allocate_no_array_at_o3() {
    assert_no_array(OptLevel::O3);
}
