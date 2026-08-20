//! The covered-forms table in `docs/wep-2026-08-19-power-assert-coverage.md`
//! states which operands each condition shape contributes to a power-assert
//! failure. This test is what keeps that table honest: one `assert` per shape,
//! checked against the plan `wado dump --assert-plan` prints.
//!
//! A shape the scanner stops descending into renders no operand, which is the
//! silent degradation WEP rule 3 forbids — so a row that goes empty here is a
//! failure, not a fixture to update.

use crate::common::InMemoryHost;
use wado_compiler::{OptLevel, dump_with_host_and_world};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

const SOURCE: &str = r#"
struct Inner {
    v: i32,
}

struct Outer {
    inner: Inner,
}

variant Shape {
    Circle(f64),
    Point,
}

fn twice(n: i32) -> i32 {
    return n * 2;
}

export fn run() {
    let a = 1;
    let b = 2;
    let s = "hello";
    let o = Outer { inner: Inner { v: 1 } };
    let list: List<i32> = [10, 20];
    let shape = Shape::Point;

    assert a < b;
    assert !(a > b);
    assert twice(a) == b;
    assert s.len() == 5;
    assert o.inner.v == 1;
    assert list[a] == 20;
    assert a as i64 == 1;
    assert 0 <= a < b;
    assert shape matches { Point };
    assert [a, b] == [1, 2];
    assert Inner { v: a } == Inner { v: 1 };
    assert (if a > 0 { a } else { b }) == 1;
    assert (match a { 1 => a, _ => b }) == 1;
    assert (a..<b).contains(&a);
    assert a < b && b < 3;
    assert a > b || b > 0;
}
"#;

/// Each entry is `(condition source, operand labels the plan must contain)`.
/// The labels are the failure message's left-hand column, in plan order.
const EXPECTED: &[(&str, &[&str])] = &[
    ("a < b", &["a", "b"]),
    ("!(a > b)", &["a", "b", "a > b"]),
    ("twice(a) == b", &["twice(a)", "b"]),
    ("s.len() == 5", &["s", "s.len()"]),
    ("o.inner.v == 1", &["o", "o.inner", "o.inner.v"]),
    ("list[a] == 20", &["list", "a", "list[a]"]),
    ("a as i64 == 1", &["a", "a as i64"]),
    ("0 <= a < b", &["a", "b"]),
    ("shape matches { Point }", &["shape"]),
    ("[a, b] == [1, 2]", &["a", "b"]),
    ("Inner { v: a } == Inner { v: 1 }", &["a"]),
    (
        "if a > 0 { a } else { b } == 1",
        &["a", "a > 0", "if a > 0 { a } else { b }"],
    ),
    (
        "match a { 1 => a, _ => b, } == 1",
        &["a", "match a { 1 => a, _ => b, }"],
    ),
    (
        "(a..<b).contains(&a)",
        &["a", "b", "a..<b", "(a..<b).contains(&a)"],
    ),
    ("a < b && b < 3", &["a", "b", "a < b", "b", "b < 3"]),
    ("a > b || b > 0", &["a", "b", "a > b", "b", "b > 0"]),
];

/// Operands whose evaluation a short-circuit can skip. The plan marks these
/// `conditional`, which is what makes the failure message say
/// `<not evaluated>` rather than quote a value the run never produced.
const EXPECTED_CONDITIONAL: &[(&str, &[&str])] = &[
    ("0 <= a < b", &["b"]),
    ("a < b && b < 3", &["b", "b < 3"]),
    ("a > b || b > 0", &["b", "b > 0"]),
];

fn entry_plan() -> String {
    let host = InMemoryHost::new();
    let dump = block_on(dump_with_host_and_world(
        SOURCE,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        None,
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    ))
    .expect("dump succeeds");

    let text = dump
        .assert_plan_text
        .expect("assert plans present after annotate");
    let entry = text
        .split("// --- Module: ")
        .find(|block| block.starts_with("entry.wado"))
        .expect("entry module has a plan block");
    entry.to_string()
}

/// The block of plan lines for one `assert`, without its header.
fn plan_of<'a>(plan: &'a str, condition: &str) -> &'a str {
    let header = format!(": assert {condition}\n");
    let start = plan
        .find(&header)
        .unwrap_or_else(|| panic!("no plan for `{condition}` in:\n{plan}"))
        + header.len();
    let rest = &plan[start..];
    let end = rest
        .find("\n1")
        .or_else(|| rest.find("\n2"))
        .or_else(|| rest.find("\n3"))
        .or_else(|| rest.find("\n4"))
        .or_else(|| rest.find("\n5"))
        .map_or(rest.len(), |i| i + 1);
    &rest[..end]
}

#[test]
fn every_condition_shape_captures_its_operands() {
    let plan = entry_plan();

    for (condition, labels) in EXPECTED {
        let block = plan_of(&plan, condition);
        for label in *labels {
            let needle = format!("  {label}\n");
            assert!(
                block.contains(&needle),
                "`{condition}` should capture `{label}`, got:\n{block}"
            );
        }
    }
}

#[test]
fn a_short_circuited_operand_is_marked_conditional() {
    let plan = entry_plan();

    for (condition, labels) in EXPECTED_CONDITIONAL {
        let block = plan_of(&plan, condition);
        for label in *labels {
            let needle = format!("conditional  {label}\n");
            assert!(
                block.contains(&needle),
                "`{condition}` should mark `{label}` conditional, got:\n{block}"
            );
        }
    }
}

#[test]
fn no_condition_shape_renders_an_empty_plan() {
    let plan = entry_plan();

    assert!(
        !plan.contains("(no operand captured)"),
        "every shape in this program has an operand to show, got:\n{plan}"
    );
}
