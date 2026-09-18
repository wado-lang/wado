//! A vector literal over constants is a constant: the `splat` + `replace_lane`
//! chain `core:simd`'s `From<Array<…>>` lowers to folds to one `v128.const`.
//!
//! Left unfolded it is rebuilt wherever it is read — once per loop iteration
//! for a multiplier a kernel holds in a local.

use std::path::Path;

use crate::common::wir_function_body;
use wado_compiler::OptLevel;

const SOURCE: &str = r#"
use { u64x2 } from "core:simd";

global MUL: u64 = 0xBF58476D1CE4E5B9;

#[inline(never)]
fn scale(key: u64x2) -> u64x2 {
    let m: u64x2 = [MUL as i64, MUL as i64];
    return key * m;
}

export fn run() {
    let k: u64x2 = [1, 2];
    assert scale(k).extract_lane(0) != 0;
}
"#;

const FILE: &str = "vector_const_fold_test.wado";

fn scale_body(opt_level: OptLevel) -> String {
    wir_function_body(
        Path::new(FILE),
        SOURCE,
        opt_level,
        &format!("fn \"{FILE}/scale\""),
    )
}

#[test]
fn a_constant_vector_literal_is_one_v128_const() {
    let body = scale_body(OptLevel::O2);
    assert!(
        body.contains("v128.const"),
        "the multiplier must be an immediate:\n{body}"
    );
    for spelling in ["i64x2.splat", "i64x2.replace_lane"] {
        assert!(
            !body.contains(spelling),
            "nothing must build the multiplier at run time, found `{spelling}`:\n{body}"
        );
    }
}
