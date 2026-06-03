//! Behavioral unit tests for the trait-query subsystem in
//! `src/elaborator/trait_query.rs`.
//!
//! These tests exercise `resolve_trait_method_for_op` and
//! `try_auto_derived_method_match` indirectly, through the elaborator's
//! public `compile_source` entry point, to lock in the invariants the
//! subsystem is responsible for:
//!
//! 1. Auto-derived `Eq` / `Ord` on user-defined structs, variants, and
//!    enums is discoverable by BOTH operator dispatch and direct method
//!    call (`p1 == p2` and `p1.eq(&p2)` must behave the same way).
//! 2. User-written generic trait impls (e.g. `impl<T: Eq, E: Eq> Eq
//!    for Result<T, E>`) have `Self` substituted concretely before
//!    argument typechecking, so mismatched arguments produce a
//!    `TypeMismatch` diagnostic rather than an ICE.
//! 3. Operator dispatch against a type that does not implement the
//!    requested trait produces a targeted "does not implement" error.
//! 4. Shift operators (`<<`, `>>`) accept `rhs: u32` verbatim — the
//!    subsystem distinguishes `&Self` from a concrete rhs type.

#![allow(unused_crate_dependencies)]

mod common;

fn compile_ok(source: &str) {
    match common::compile_source(source) {
        Ok(_) => {}
        Err(e) => panic!("expected successful compile, got error: {e}"),
    }
}

fn compile_err_contains(source: &str, needle: &str) -> String {
    match common::compile_source(source) {
        Ok(_) => panic!("expected compile error containing {needle:?}, but compile succeeded"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(needle),
                "expected error containing {needle:?}, got: {msg}"
            );
            msg
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-derived Eq/Ord on plain structs
// ---------------------------------------------------------------------------

#[test]
fn auto_derived_eq_operator_on_struct() {
    compile_ok(
        r"
struct P { x: i32 }

export fn run() {
    let a = P { x: 1 };
    let b = P { x: 1 };
    assert a == b;
}
",
    );
}

#[test]
fn auto_derived_eq_method_call_on_struct() {
    // Before stage 2, `p.eq(&p2)` on a user struct failed with
    // "no method 'eq' found on type 'P'" because
    // find_trait_method_for_type ignored auto-derive eligibility.
    compile_ok(
        r"
struct P { x: i32 }

export fn run() {
    let a = P { x: 1 };
    let b = P { x: 1 };
    assert a.eq(&b);
}
",
    );
}

#[test]
fn auto_derived_eq_method_rejects_wrong_arg_type() {
    // The synthesized TraitMethodMatch carries concrete param_types
    // = [&P], so resolve_method_call's arg typecheck catches the
    // mismatch at resolve time (no ICE).
    compile_err_contains(
        r"
struct P { x: i32 }

export fn run() {
    let a = P { x: 1 };
    let _ = a.eq(&42);
}
",
        "type mismatch",
    );
}

#[test]
fn auto_derived_ord_operator_on_struct() {
    compile_ok(
        r"
struct P { x: i32 }

export fn run() {
    let a = P { x: 1 };
    let b = P { x: 2 };
    assert a < b;
    assert !(a > b);
}
",
    );
}

// ---------------------------------------------------------------------------
// Self substitution in user-written generic trait impls
// ---------------------------------------------------------------------------

#[test]
fn user_generic_eq_direct_method_call_succeeds() {
    // Tests that `impl<T: Eq, E: Eq> Eq for Result<T, E>` (declared in
    // core:prelude) resolves to a concretely-typed method when called
    // directly.  Uses the Result type from the prelude so no explicit
    // impl is needed in the test source.
    compile_ok(
        r"
export fn run() {
    let r1: Result<i32, i32> = Result::Ok(1);
    let r2: Result<i32, i32> = Result::Ok(1);
    assert r1.eq(&r2);
}
",
    );
}

#[test]
fn user_generic_eq_direct_method_rejects_wrong_type() {
    // Before stage 3, `&Self` in the impl decl resolved to
    // `TypeTable::UNKNOWN`, so the argument typecheck silently
    // accepted mismatches and ICEd at Wasm validation.  Now `Self`
    // is substituted to the concrete receiver type.
    let msg = compile_err_contains(
        r#"
export fn run() {
    let r: Result<String, i32> = Result::Ok("hi");
    let _ = r.eq(&42);
}
"#,
        "type mismatch",
    );
    assert!(
        msg.contains("Result<String, i32>"),
        "expected substituted receiver type in error, got: {msg}"
    );
}

#[test]
fn user_generic_eq_operator_rejects_wrong_type() {
    // Same guarantee, via operator dispatch.
    compile_err_contains(
        r#"
export fn run() {
    let r: Result<String, i32> = Result::Ok("hi");
    let _ = r == "";
}
"#,
        "type mismatch",
    );
}

// ---------------------------------------------------------------------------
// Type-parameter-bounded Eq dispatch
// ---------------------------------------------------------------------------

#[test]
fn type_param_bounded_eq_dispatch() {
    // A `T: Eq` bound allows `==` inside a generic function; the
    // operator path goes through the type-param branch of
    // resolve_binary.
    compile_ok(
        r"
fn both_equal<T: Eq>(a: T, b: T, c: T) -> bool {
    return a == b && b == c;
}

export fn run() {
    let ok = both_equal::<i32>(1, 1, 1);
    assert ok;
}
",
    );
}

// ---------------------------------------------------------------------------
// Shift dispatches rhs:u32 verbatim (not wrapped in &Self)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Missing trait reports a clean diagnostic
// ---------------------------------------------------------------------------

#[test]
fn operator_on_fn_type_gives_targeted_error() {
    // Function types have no native Wasm binary-op support and no trait
    // impl; resolve_trait_method_for_op must return None and
    // resolve_binary's "requires_trait" fallthrough must reject before
    // reaching the primitive `TirExprKind::Binary` construction.  Before
    // the fix in stage 7a this ICEd at codegen validation.
    compile_err_contains(
        r"
fn takes_fn(f: fn() -> i32, g: fn() -> i32) -> bool {
    return f == g;
}

export fn run() {
    let _ = takes_fn(|| 1, || 2);
}
",
        "cannot be applied",
    );
}

#[test]
fn operator_error_is_not_labeled_invalid_pattern() {
    // Operator type errors must read as operator errors, not as
    // "invalid pattern:" — the latter is reserved for actual pattern
    // contexts (match arms, `if let`, destructuring). Routing operator
    // errors through `InvalidPattern` mislabeled them.
    let msg = compile_err_contains(
        r#"
export fn run() {
    let _ = "x" - 3;
}
"#,
        "cannot be applied",
    );
    assert!(
        !msg.contains("invalid pattern"),
        "operator error must not be labeled `invalid pattern`, got: {msg}"
    );
}

#[test]
fn operator_mismatch_is_symmetric_in_operand_order() {
    // A binary operator applied to two incompatible types must produce
    // the same kind of message regardless of which operand is the
    // non-primitive one. Previously `a - lst` (primitive lhs) reported a
    // `TypeMismatch` while `lst - a` (non-primitive lhs) reported an
    // operator error, so the same defect read completely differently
    // depending on operand order.
    let left_primitive = compile_err_contains(
        r"
fn f(a: i32, lst: List<i32>) -> i32 { return a - lst; }
export fn run() { let _ = f(1, [2, 3]); }
",
        "cannot be applied",
    );
    let left_non_primitive = compile_err_contains(
        r"
fn f(a: i32, lst: List<i32>) -> i32 { return lst - a; }
export fn run() { let _ = f(1, [2, 3]); }
",
        "cannot be applied",
    );
    // Both name the two operand types.
    assert!(
        left_primitive.contains("i32") && left_primitive.contains("List<i32>"),
        "expected both operand types, got: {left_primitive}"
    );
    assert!(
        left_non_primitive.contains("i32") && left_non_primitive.contains("List<i32>"),
        "expected both operand types, got: {left_non_primitive}"
    );
}

#[test]
fn trait_bound_error_explains_offending_field() {
    // When a `T: Ord` bound is unsatisfied, the diagnostic must explain WHY:
    // name the struct field whose type breaks the auto-derive, not just state
    // that the type "does not implement Ord".
    let msg = compile_err_contains(
        r"
struct Handler { cb: fn(i32) -> i32 }
fn smallest<T: Ord>(a: T, b: T) -> T {
    if a < b { return a; }
    return b;
}
export fn run() {
    let _ = smallest(Handler { cb: |x: i32| x }, Handler { cb: |x: i32| x });
}
",
        "does not implement trait 'Ord'",
    );
    assert!(
        msg.contains("note:") && msg.contains("field `cb`"),
        "expected a reason chain naming field `cb`, got: {msg}"
    );
}

#[test]
fn trait_bound_error_reason_chain_is_recursive() {
    // The reason chain unfolds through nested structs: Outer fails because
    // field `inner: Inner` fails, which in turn fails because field `f` is a
    // function type.
    let msg = compile_err_contains(
        r"
struct Inner { f: fn() -> i32 }
struct Outer { inner: Inner }
fn smallest<T: Ord>(a: T, b: T) -> T {
    if a < b { return a; }
    return b;
}
export fn run() {
    let _ = smallest(Outer { inner: Inner { f: || 1 } }, Outer { inner: Inner { f: || 2 } });
}
",
        "does not implement trait 'Ord'",
    );
    assert!(
        msg.contains("field `inner`") && msg.contains("field `f`"),
        "expected a recursive reason chain (Outer -> inner -> f), got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Unary trait operators (Neg, BitNot) go through the same subsystem
// ---------------------------------------------------------------------------

#[test]
fn unary_neg_dispatches_through_trait_subsystem() {
    // i128 has `impl Neg for i128` in the prelude — exercise that the
    // unary operator dispatch lands in the same resolve_trait_method_for_op
    // pipeline as binary operators.  The builder's zero-arg branch
    // (`resolved.param_types.is_empty()`) runs for this case.
    compile_ok(
        r"
export fn run() {
    let x: i128 = 5;
    let y: i128 = -x;
    assert y == (-(5 as i128));
}
",
    );
}

#[test]
fn unary_bitnot_dispatches_through_trait_subsystem() {
    compile_ok(
        r"
export fn run() {
    let x: i128 = 0;
    let y: i128 = ~x;
    assert y == (-(1 as i128));
}
",
    );
}

#[test]
fn shift_operator_accepts_u32_rhs() {
    // i128 has `impl Shl for i128 { fn shl(&self, rhs: u32) -> i128 }`
    // in the prelude — use that to confirm the subsystem accepts
    // concrete rhs types (not `&Self`) without wrapping in `&`.
    compile_ok(
        r"
export fn run() {
    let x: i128 = 1;
    let shifted: i128 = x << 4;
    assert shifted == (16 as i128);
}
",
    );
}
