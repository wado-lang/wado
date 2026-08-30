//! `nir/cold_outline` moves what a `cold_path()` marker opens into a function
//! of its own, so the inline cost model's cold discount describes the callee
//! rather than promising a split nobody made.
//!
//! What the split has to preserve is the whole reason the region was written:
//! the assertion still fires, with its operands, from the function it moved to.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

/// The archetype: a hot append whose growth arm carries several calls and a
/// power-assert. The arm's only free variable is the `&mut` receiver, and the
/// assert's capture slots are function locals nothing outside the arm reads.
const SOURCE: &str = r#"
struct Cols { a: List<i32>, b: List<i32>, c: List<i32> }

impl Cols {
    fn push_row(&mut self, x: i32) {
        if self.a.len() >= self.a.capacity() {
            builtin::cold_path();
            self.a.reserve(8);
            self.b.reserve(8);
            self.c.reserve(8);
            assert self.b.capacity() == self.a.capacity()
                && self.c.capacity() == self.a.capacity(), "columns grow in lockstep";
        }
        self.a.push(x);
        self.b.push(x);
        self.c.push(x);
    }
}

export fn run() {
    let mut cols = Cols { a: [], b: [], c: [] };
    let mut i = 0;
    while i < 50 { cols.push_row(i); i = i + 1 }
    assert cols.a.len() == 50 && cols.c.len() == 50;
}
"#;

/// A region control can leave is not one that can become a call: `return` in a
/// function of its own returns from the wrong frame.
const RETURNING_REGION: &str = r#"
fn find(xs: &List<i32>, key: i32) -> i32 {
    if xs.len() == 0 {
        builtin::cold_path();
        let sentinel = 0 - 1;
        return sentinel;
    }
    let mut i = 0;
    while i < xs.len() {
        if xs[i] == key { return i }
        i = i + 1
    }
    return 0 - 1;
}

export fn run() {
    let xs: List<i32> = [3, 1, 4];
    let empty: List<i32> = [];
    assert find(&xs, 4) == 2;
    assert find(&empty, 4) == 0 - 1;
}
"#;

fn compile(source: &str, opt_level: OptLevel) -> wado_compiler::CompileResult {
    let options = CompilerOptions {
        opt_level,
        retain_wir: true,
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(
        Path::new("cold_outline_test.wado"),
        source,
        options,
    )
    .expect("compilation should succeed")
}

fn wir_of(source: &str, opt_level: OptLevel) -> String {
    crate::common::wir_text(Path::new("cold_outline_test.wado"), source, opt_level)
}

#[test]
fn cold_region_moves_out_of_its_enclosing_function() {
    let wir = wir_of(SOURCE, OptLevel::O2);
    assert!(
        wir.contains("push_row$cold0"),
        "the growth arm should have moved into a helper"
    );
    // The point of moving it: what the caller copies is the hot path. The four
    // `reserve`/`grow` calls and the assert's formatting live behind one call.
    let helper_calls = wir.matches("push_row$cold0").count();
    assert!(
        helper_calls >= 2,
        "expected a definition and a call site, found {helper_calls}"
    );
}

/// The helper is a real function, not a renaming: the program still grows the
/// columns and still passes the assertion that lives inside the moved region —
/// at every level, split or not.
#[test]
fn the_split_program_still_runs() {
    for level in [OptLevel::O0, OptLevel::O1, OptLevel::O2, OptLevel::O3] {
        crate::common::run_wasm(compile(SOURCE, level).wasm)
            .unwrap_or_else(|e| panic!("{level:?} should run: {e}"));
    }
}

#[test]
fn a_region_that_returns_stays_put() {
    let wir = wir_of(RETURNING_REGION, OptLevel::O2);
    assert!(
        !wir.contains("find$cold"),
        "a region containing `return` must not be moved"
    );
}
