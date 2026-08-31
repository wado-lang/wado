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

/// A parameter rides in as itself, so a write to its slot is one no call carries
/// back: the helper would assign its own frame and the enclosing function keep
/// the value it was called with.
const WRITTEN_PARAM: &str = r#"
#[inline(never)]
fn work(x: i32) -> i32 {
    return x * 3;
}

fn scale(mut n: i32, cond: bool) -> i32 {
    if cond {
        builtin::cold_path();
        n = work(n) + work(n + 1) + work(n + 2);
    }
    return n;
}

export fn run() {
    assert scale(2, true) == 27;
    assert scale(2, false) == 2;
}
"#;

const PATH: &str = "cold_outline_test.wado";

fn wir_of(source: &str, opt_level: OptLevel) -> String {
    crate::common::wir_text(Path::new(PATH), source, opt_level)
}

/// The assertions inside `source` hold at every level, split or not — which is
/// what makes the helper a real function rather than a renaming.
fn runs_at_every_level(source: &str) {
    for opt_level in [OptLevel::O0, OptLevel::O1, OptLevel::O2, OptLevel::O3] {
        let options = CompilerOptions {
            opt_level,
            ..Default::default()
        };
        let wasm =
            crate::common::compile_source_with_compiler_options(Path::new(PATH), source, options)
                .expect("compilation should succeed")
                .wasm;
        crate::common::run_wasm(wasm).unwrap_or_else(|e| panic!("{opt_level:?} should run: {e}"));
    }
}

/// A region the preconditions turn down leaves no helper behind.
fn stays_put(source: &str, enclosing: &str) {
    let wir = wir_of(source, OptLevel::O2);
    assert!(
        !wir.contains(&format!("{enclosing}$cold")),
        "`{enclosing}`'s region must not be moved:\n{wir}"
    );
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

#[test]
fn the_split_program_still_runs() {
    runs_at_every_level(SOURCE);
}

#[test]
fn a_region_that_returns_stays_put() {
    stays_put(RETURNING_REGION, "find");
}

#[test]
fn a_region_that_writes_a_parameter_stays_put() {
    stays_put(WRITTEN_PARAM, "scale");
}

#[test]
fn a_written_parameter_keeps_its_value() {
    runs_at_every_level(WRITTEN_PARAM);
}
