//! Integration tests for [`wado_compiler::tiri`] — the TIR interpreter
//! that powers constant folding (and, eventually, branch / loop reduction).
//!
//! Each test builds a tiny TIR expression, runs it through
//! [`Interpreter::reduce_to_lattice`], and checks the resulting
//! [`Lattice`]. The focus here is the four arithmetic ops across a
//! handful of representative integer / float types — the goal being to
//! give the interpreter a stable contract to refactor against, not to
//! enumerate every operator.

use wado_compiler::Span;
use wado_compiler::tir::{
    PrimitiveType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};
use wado_compiler::tiri::{Interpreter, Lattice, Value};

fn int_lit(value: u64, type_id: TypeId, repr: &str) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            repr: repr.to_string(),
            value,
        },
        type_id,
        Span::default(),
    )
}

fn float_lit(value: f64, type_id: TypeId, repr: &str) -> TirExpr {
    TirExpr::new(
        TirExprKind::FloatLiteral {
            repr: repr.to_string(),
            value,
        },
        type_id,
        Span::default(),
    )
}

fn bool_lit(value: bool) -> TirExpr {
    TirExpr::new(
        TirExprKind::BoolLiteral(value),
        TypeTable::BOOL,
        Span::default(),
    )
}

fn binary(op: TirBinaryOp, left: TirExpr, right: TirExpr, result_ty: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        },
        result_ty,
        Span::default(),
    )
}

fn unary(op: TirUnaryOp, expr: TirExpr, result_ty: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Unary {
            op,
            expr: Box::new(expr),
        },
        result_ty,
        Span::default(),
    )
}

fn local_expr(index: u32, type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: format!("l{index}"),
        },
        type_id,
        Span::default(),
    )
}

/// Convenience wrapper used by the legacy "is this a Const?" tests: run
/// `reduce_to_lattice` and project to `Option<Value>` via
/// [`Lattice::as_const`]. Unevaluated and `NonConst` both collapse to
/// `None` here — when a test cares about the distinction it pattern
/// matches on [`Lattice`] directly.
fn eval(expr: &TirExpr) -> Option<Value> {
    let table = TypeTable::new();
    Interpreter::new(&table).reduce_to_lattice(expr).as_const()
}

fn expect_int(expr: &TirExpr, expected_value: u64, expected_prim: PrimitiveType) {
    let v = eval(expr).expect("expected reduction");
    assert_eq!(
        v,
        Value::Int {
            value: expected_value,
            prim: expected_prim,
        },
        "got {v:?}"
    );
}

fn expect_float(expr: &TirExpr, expected_value: f64, expected_prim: PrimitiveType) {
    let v = eval(expr).expect("expected reduction");
    match v {
        Value::Float { value, prim } => {
            assert_eq!(prim, expected_prim, "wrong float width");
            assert_eq!(value, expected_value, "wrong float value");
        }
        other => panic!("expected float, got {other:?}"),
    }
}

fn expect_bool(expr: &TirExpr, expected: bool) {
    assert_eq!(eval(expr), Some(Value::Bool(expected)));
}

// ──────────────────────────────────────────────────────────────────────────────
// i32 — full four-op coverage
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn i32_add() {
    let e = binary(
        TirBinaryOp::Add,
        int_lit(20, TypeTable::I32, "20"),
        int_lit(22, TypeTable::I32, "22"),
        TypeTable::I32,
    );
    expect_int(&e, 42, PrimitiveType::I32);
}

#[test]
fn i32_sub_negative_result() {
    let e = binary(
        TirBinaryOp::Sub,
        int_lit(3, TypeTable::I32, "3"),
        int_lit(10, TypeTable::I32, "10"),
        TypeTable::I32,
    );
    // -7 sign-extended to u64
    expect_int(&e, (-7_i32) as u64, PrimitiveType::I32);
}

#[test]
fn i32_mul() {
    let e = binary(
        TirBinaryOp::Mul,
        int_lit(6, TypeTable::I32, "6"),
        int_lit(7, TypeTable::I32, "7"),
        TypeTable::I32,
    );
    expect_int(&e, 42, PrimitiveType::I32);
}

#[test]
fn i32_div_truncates_toward_zero() {
    // -7 / 2 == -3 (truncation, matching Wasm i32.div_s)
    let e = binary(
        TirBinaryOp::Div,
        int_lit((-7_i32) as u64, TypeTable::I32, "-7"),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    );
    expect_int(&e, (-3_i32) as u64, PrimitiveType::I32);
}

#[test]
fn i32_div_by_zero_is_unreducible() {
    let e = binary(
        TirBinaryOp::Div,
        int_lit(42, TypeTable::I32, "42"),
        int_lit(0, TypeTable::I32, "0"),
        TypeTable::I32,
    );
    assert_eq!(eval(&e), None, "div-by-zero must preserve the runtime trap");
}

#[test]
fn i32_div_min_by_neg_one_is_unreducible() {
    let e = binary(
        TirBinaryOp::Div,
        int_lit(u64::from(i32::MIN as u32), TypeTable::I32, "-2147483648"),
        int_lit((-1_i32) as u64, TypeTable::I32, "-1"),
        TypeTable::I32,
    );
    assert_eq!(
        eval(&e),
        None,
        "i32::MIN / -1 must preserve the runtime trap"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// i64
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn i64_arithmetic() {
    let add = binary(
        TirBinaryOp::Add,
        int_lit(1_000_000_000_000, TypeTable::I64, "1000000000000"),
        int_lit(1, TypeTable::I64, "1"),
        TypeTable::I64,
    );
    expect_int(&add, 1_000_000_000_001, PrimitiveType::I64);

    let mul = binary(
        TirBinaryOp::Mul,
        int_lit(1_000_000, TypeTable::I64, "1000000"),
        int_lit(1_000_000, TypeTable::I64, "1000000"),
        TypeTable::I64,
    );
    expect_int(&mul, 1_000_000_000_000, PrimitiveType::I64);
}

// ──────────────────────────────────────────────────────────────────────────────
// u8 — wrapping behaviour
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn u8_add_wraps() {
    let e = binary(
        TirBinaryOp::Add,
        int_lit(255, TypeTable::U8, "255"),
        int_lit(1, TypeTable::U8, "1"),
        TypeTable::U8,
    );
    expect_int(&e, 0, PrimitiveType::U8);
}

#[test]
fn u8_sub_wraps() {
    let e = binary(
        TirBinaryOp::Sub,
        int_lit(0, TypeTable::U8, "0"),
        int_lit(1, TypeTable::U8, "1"),
        TypeTable::U8,
    );
    expect_int(&e, 255, PrimitiveType::U8);
}

#[test]
fn u8_div_unsigned() {
    let e = binary(
        TirBinaryOp::Div,
        int_lit(200, TypeTable::U8, "200"),
        int_lit(3, TypeTable::U8, "3"),
        TypeTable::U8,
    );
    expect_int(&e, 66, PrimitiveType::U8);
}

// ──────────────────────────────────────────────────────────────────────────────
// u32 / mod
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn u32_mod() {
    let e = binary(
        TirBinaryOp::Mod,
        int_lit(100, TypeTable::U32, "100"),
        int_lit(7, TypeTable::U32, "7"),
        TypeTable::U32,
    );
    expect_int(&e, 2, PrimitiveType::U32);
}

// ──────────────────────────────────────────────────────────────────────────────
// f64 / f32 — four-op coverage
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn f64_add() {
    let e = binary(
        TirBinaryOp::Add,
        float_lit(1.5, TypeTable::F64, "1.5"),
        float_lit(2.5, TypeTable::F64, "2.5"),
        TypeTable::F64,
    );
    expect_float(&e, 4.0, PrimitiveType::F64);
}

#[test]
fn f64_sub() {
    let e = binary(
        TirBinaryOp::Sub,
        float_lit(10.0, TypeTable::F64, "10.0"),
        float_lit(3.5, TypeTable::F64, "3.5"),
        TypeTable::F64,
    );
    expect_float(&e, 6.5, PrimitiveType::F64);
}

#[test]
fn f64_mul() {
    let e = binary(
        TirBinaryOp::Mul,
        float_lit(3.0, TypeTable::F64, "3.0"),
        float_lit(2.0, TypeTable::F64, "2.0"),
        TypeTable::F64,
    );
    expect_float(&e, 6.0, PrimitiveType::F64);
}

#[test]
fn f64_div() {
    let e = binary(
        TirBinaryOp::Div,
        float_lit(10.0, TypeTable::F64, "10.0"),
        float_lit(4.0, TypeTable::F64, "4.0"),
        TypeTable::F64,
    );
    expect_float(&e, 2.5, PrimitiveType::F64);
}

#[test]
fn f64_div_by_zero_is_infinity() {
    // 1/0 in IEEE 754 is +Infinity (not NaN), so it folds.
    let e = binary(
        TirBinaryOp::Div,
        float_lit(1.0, TypeTable::F64, "1.0"),
        float_lit(0.0, TypeTable::F64, "0.0"),
        TypeTable::F64,
    );
    expect_float(&e, f64::INFINITY, PrimitiveType::F64);
}

#[test]
fn f64_zero_div_zero_is_nan_unreducible() {
    // 0/0 is NaN; nondeterministic NaN bits → don't fold.
    let e = binary(
        TirBinaryOp::Div,
        float_lit(0.0, TypeTable::F64, "0.0"),
        float_lit(0.0, TypeTable::F64, "0.0"),
        TypeTable::F64,
    );
    assert_eq!(eval(&e), None);
}

#[test]
fn f32_add_uses_f32_precision() {
    // 1/3 differs between f32 and f64; round-trip the operands as f32.
    let e = binary(
        TirBinaryOp::Div,
        float_lit(1.0, TypeTable::F32, "1.0"),
        float_lit(3.0, TypeTable::F32, "3.0"),
        TypeTable::F32,
    );
    let expected = f64::from(1.0_f32 / 3.0_f32);
    expect_float(&e, expected, PrimitiveType::F32);
}

// ──────────────────────────────────────────────────────────────────────────────
// Unary ops
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn i32_neg() {
    let e = unary(
        TirUnaryOp::Neg,
        int_lit(42, TypeTable::I32, "42"),
        TypeTable::I32,
    );
    expect_int(&e, (-42_i32) as u64, PrimitiveType::I32);
}

#[test]
fn u32_neg_is_unreducible() {
    // Negation is undefined on unsigned ints — must not fold.
    let e = unary(
        TirUnaryOp::Neg,
        int_lit(42, TypeTable::U32, "42"),
        TypeTable::U32,
    );
    assert_eq!(eval(&e), None);
}

#[test]
fn f64_neg_zero() {
    let e = unary(
        TirUnaryOp::Neg,
        float_lit(0.0, TypeTable::F64, "0.0"),
        TypeTable::F64,
    );
    let v = eval(&e).expect("negation always folds for floats");
    let Value::Float { value, prim } = v else {
        panic!("expected float, got {v:?}");
    };
    assert_eq!(prim, PrimitiveType::F64);
    assert!(
        value == 0.0 && value.is_sign_negative(),
        "expected -0.0, got {value}"
    );
}

#[test]
fn bool_not() {
    let e = unary(TirUnaryOp::Not, bool_lit(true), TypeTable::BOOL);
    expect_bool(&e, false);
}

// ──────────────────────────────────────────────────────────────────────────────
// Nested expressions — interpreter recurses through children
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn nested_int_arithmetic() {
    // (2 + 3) * (10 - 4) == 30
    let lhs = binary(
        TirBinaryOp::Add,
        int_lit(2, TypeTable::I32, "2"),
        int_lit(3, TypeTable::I32, "3"),
        TypeTable::I32,
    );
    let rhs = binary(
        TirBinaryOp::Sub,
        int_lit(10, TypeTable::I32, "10"),
        int_lit(4, TypeTable::I32, "4"),
        TypeTable::I32,
    );
    let e = binary(TirBinaryOp::Mul, lhs, rhs, TypeTable::I32);
    expect_int(&e, 30, PrimitiveType::I32);
}

#[test]
fn comparison_yields_bool() {
    let e = binary(
        TirBinaryOp::Lt,
        int_lit(3, TypeTable::I32, "3"),
        int_lit(5, TypeTable::I32, "5"),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

// ──────────────────────────────────────────────────────────────────────────────
// Non-foldable input — reduce_to_lattice returns Unevaluated/NonConst
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn non_literal_operand_is_unreducible() {
    // A `Unit` expression as the operand stands in for any non-literal —
    // the interpreter has no Value to produce, so the result is not
    // `Const`. (`Unit` is structurally outside the engine's model, so
    // it lattice-resolves to Unevaluated, which propagates through the
    // surrounding Binary.)
    let e = binary(
        TirBinaryOp::Add,
        int_lit(1, TypeTable::I32, "1"),
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, Span::default()),
        TypeTable::I32,
    );
    assert_eq!(eval(&e), None);
}

// ──────────────────────────────────────────────────────────────────────────────
// `reduce` returns a TirExpr — repr preservation and shape contracts
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn reduce_preserves_literal_repr() {
    // A bare `0xFF` IntLiteral must reduce to itself with the original
    // hex repr intact — fold passes must never round-trip leaf literals
    // through decimal formatting.
    let table = TypeTable::new();
    let lit = int_lit(0xFF, TypeTable::U8, "0xFF");
    let reduced = Interpreter::new(&table).reduce(&lit);
    match reduced.kind {
        TirExprKind::IntLiteral { repr, value } => {
            assert_eq!(repr, "0xFF");
            assert_eq!(value, 0xFF);
        }
        other => panic!("expected IntLiteral, got {other:?}"),
    }
}

#[test]
fn reduce_collapses_binary_to_literal() {
    let table = TypeTable::new();
    let e = binary(
        TirBinaryOp::Add,
        int_lit(20, TypeTable::I32, "20"),
        int_lit(22, TypeTable::I32, "22"),
        TypeTable::I32,
    );
    let reduced = Interpreter::new(&table).reduce(&e);
    match reduced.kind {
        TirExprKind::IntLiteral { value, .. } => assert_eq!(value, 42),
        other => panic!("expected IntLiteral after fold, got {other:?}"),
    }
}

#[test]
fn reduce_short_circuits_or_false() {
    // `false || X` reduces to `X` even when `X` is non-constant.
    let table = TypeTable::new();
    let lhs = bool_lit(false);
    let rhs = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, Span::default());
    let e = binary(TirBinaryOp::Or, lhs, rhs, TypeTable::BOOL);
    let reduced = Interpreter::new(&table).reduce(&e);
    assert!(
        matches!(reduced.kind, TirExprKind::Unit),
        "false || X should reduce to X, got {:?}",
        reduced.kind
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Lattice API — three states are observable, projection collapses two
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn lattice_const_for_literal() {
    let table = TypeTable::new();
    let lit = int_lit(42, TypeTable::I32, "42");
    let lat = Interpreter::new(&table).reduce_to_lattice(&lit);
    assert!(matches!(
        lat,
        Lattice::Const(Value::Int {
            value: 42,
            prim: PrimitiveType::I32,
        }),
    ));
}

#[test]
fn lattice_unevaluated_for_unsupported_kind() {
    // A bare `Unit` is outside the engine's model — distinct from
    // `NonConst`, which is reserved for things provably non-constant.
    let table = TypeTable::new();
    let e = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, Span::default());
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&e),
        Lattice::Unevaluated,
    );
}

#[test]
fn lattice_unevaluated_for_unbound_local() {
    // No bind_local call → reading the local is "I don't know yet",
    // not "I know it isn't const".
    let table = TypeTable::new();
    let local = local_expr(0, TypeTable::I32);
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&local),
        Lattice::Unevaluated,
    );
}

#[test]
fn lattice_nonconst_for_div_by_zero() {
    // Both operands are Const, but the op evidently fails — that's
    // NonConst, distinct from Unevaluated.
    let table = TypeTable::new();
    let e = binary(
        TirBinaryOp::Div,
        int_lit(1, TypeTable::I32, "1"),
        int_lit(0, TypeTable::I32, "0"),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&e),
        Lattice::NonConst,
    );
}

#[test]
fn lattice_as_const_collapses_unevaluated_and_nonconst() {
    // The `as_const` projection is a one-way door into Option<Value>:
    // both Unevaluated and NonConst become None, exactly the loss of
    // information that callers of `as_const` opt in to.
    assert!(Lattice::Unevaluated.as_const().is_none());
    assert!(Lattice::NonConst.as_const().is_none());
    assert_eq!(
        Lattice::Const(Value::Bool(true)).as_const(),
        Some(Value::Bool(true)),
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Stage 1 — local-variable env: bind_local, invalidate_local, function reset
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn bound_local_folds_in_arithmetic() {
    // env: x = Const(5).
    // Then `x + 3` reduces to Const(8), even though `x` syntactically
    // is a Local node.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
    let e = binary(
        TirBinaryOp::Add,
        local_expr(0, TypeTable::I32),
        int_lit(3, TypeTable::I32, "3"),
        TypeTable::I32,
    );
    assert_eq!(
        interp.reduce_to_lattice(&e),
        Lattice::Const(Value::Int {
            value: 8,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn nonconst_local_blocks_fold() {
    // env: x = NonConst (e.g. `let mut x = …` or post-assign).
    // `x + 3` cannot be folded; the result is NonConst.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(0, Lattice::NonConst);
    let e = binary(
        TirBinaryOp::Add,
        local_expr(0, TypeTable::I32),
        int_lit(3, TypeTable::I32, "3"),
        TypeTable::I32,
    );
    assert_eq!(interp.reduce_to_lattice(&e), Lattice::NonConst);
}

#[test]
fn invalidate_local_overrides_prior_const() {
    // x first bound to Const(5), then invalidated by an assignment.
    // Subsequent reads should see NonConst, never the stale Const.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        7,
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
    interp.invalidate_local(7);
    let e = local_expr(7, TypeTable::I32);
    assert_eq!(interp.reduce_to_lattice(&e), Lattice::NonConst);
}

#[test]
fn enter_function_clears_env() {
    // Simulates the visitor moving from one function body to the
    // next — local indices are unique per function, so prior bindings
    // must not leak.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        3,
        Lattice::Const(Value::Int {
            value: 99,
            prim: PrimitiveType::I32,
        }),
    );
    interp.enter_function();
    let e = local_expr(3, TypeTable::I32);
    assert_eq!(interp.reduce_to_lattice(&e), Lattice::Unevaluated);
}

#[test]
fn local_node_itself_is_not_rewritten_in_place() {
    // A Local with env = Const should be readable via reduce_to_lattice
    // but NOT mutated when seen on its own. This protects assignment
    // LHS targets from being rewritten into literals (which would
    // produce malformed TIR).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
    let local = local_expr(0, TypeTable::I32);
    let reduced = interp.reduce(&local);
    assert!(
        matches!(reduced.kind, TirExprKind::Local { index: 0, .. }),
        "Local must stay structurally a Local; env lookup happens at parents only, got {:?}",
        reduced.kind,
    );
}

#[test]
fn nested_const_locals_chain() {
    // env: x = Const(5), y = Const(3).
    // (x + y) * 2 reduces to Const(16).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
    interp.bind_local(
        1,
        Lattice::Const(Value::Int {
            value: 3,
            prim: PrimitiveType::I32,
        }),
    );
    let sum = binary(
        TirBinaryOp::Add,
        local_expr(0, TypeTable::I32),
        local_expr(1, TypeTable::I32),
        TypeTable::I32,
    );
    let e = binary(
        TirBinaryOp::Mul,
        sum,
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    );
    assert_eq!(
        interp.reduce_to_lattice(&e),
        Lattice::Const(Value::Int {
            value: 16,
            prim: PrimitiveType::I32,
        }),
    );
}

/// Assert that `Cast{Local{0}, target_ty}` folds to
/// `Const(Int{expected_value, target_prim})` when env binds local 0
/// to `Const(Int{src_value, src_prim})`. Exercises the Stage 1
/// env-resolved cast path that the previous regression silently
/// corrupted.
fn check_env_cast(
    src_value: u64,
    src_prim: PrimitiveType,
    src_ty: TypeId,
    target_ty: TypeId,
    target_prim: PrimitiveType,
    expected_value: u64,
) {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: src_value,
            prim: src_prim,
        }),
    );
    let cast_expr = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(local_expr(0, src_ty)),
            target_type: target_ty,
        },
        target_ty,
        Span::default(),
    );
    assert_eq!(
        interp.reduce_to_lattice(&cast_expr),
        Lattice::Const(Value::Int {
            value: expected_value,
            prim: target_prim,
        }),
        "{src_prim:?}({src_value:#x}) as {target_prim:?}",
    );
}

#[test]
fn env_cast_int_variants_match_literal_leaf_path() {
    // Cross-prim cast through an env-resolved Local must match what
    // the literal-leaf path produces, for every relevant integer
    // pairing. Each row exercises a different arithmetic intent —
    // sign-extension, zero-extension, narrowing, and reinterpret —
    // so a regression in `cast_int` for any prim shows up here
    // before it reaches an e2e fixture.

    // Sign-extension widening: i8(-1) as i32 → -1 (sign-extended)
    let neg_one_i64 = i64::from(-1_i8) as u64;
    check_env_cast(
        neg_one_i64,
        PrimitiveType::I8,
        TypeTable::I8,
        TypeTable::I32,
        PrimitiveType::I32,
        i64::from(-1_i32) as u64,
    );

    // Sign-extension widening: i8(-1) as i64
    check_env_cast(
        neg_one_i64,
        PrimitiveType::I8,
        TypeTable::I8,
        TypeTable::I64,
        PrimitiveType::I64,
        -1_i64 as u64,
    );

    // Zero-extension widening: u8(0xFF) as i32 → 255 (positive)
    check_env_cast(
        0xFF,
        PrimitiveType::U8,
        TypeTable::U8,
        TypeTable::I32,
        PrimitiveType::I32,
        255,
    );

    // Zero-extension widening: u8(0xFF) as u32 → 255
    check_env_cast(
        0xFF,
        PrimitiveType::U8,
        TypeTable::U8,
        TypeTable::U32,
        PrimitiveType::U32,
        255,
    );

    // Narrowing: i32(0x1234_5678) as i8 → 0x78 (sign-extended → 0x78
    // since the high bit is clear)
    check_env_cast(
        0x1234_5678,
        PrimitiveType::I32,
        TypeTable::I32,
        TypeTable::I8,
        PrimitiveType::I8,
        0x78,
    );

    // Narrowing with sign-flip: i32(0x1234_5680) as i8 → -128 (high
    // bit of the truncated byte is set, sign-extended)
    check_env_cast(
        0x1234_5680,
        PrimitiveType::I32,
        TypeTable::I32,
        TypeTable::I8,
        PrimitiveType::I8,
        i64::from(-128_i8) as u64,
    );

    // Narrowing: i64(0x1_FFFF_FFFF) as i32 → -1 (lower 32 bits =
    // 0xFFFF_FFFF, sign-extended back to i64)
    check_env_cast(
        0x1_FFFF_FFFF,
        PrimitiveType::I64,
        TypeTable::I64,
        TypeTable::I32,
        PrimitiveType::I32,
        i64::from(-1_i32) as u64,
    );

    // Same-width reinterpret: i32(-1) as u32 → 0xFFFF_FFFF
    check_env_cast(
        i64::from(-1_i32) as u64,
        PrimitiveType::I32,
        TypeTable::I32,
        TypeTable::U32,
        PrimitiveType::U32,
        0xFFFF_FFFF,
    );

    // Same-width reinterpret: u32(0xFFFF_FFFF) as i32 → -1 (the
    // case the original regression first surfaced on)
    check_env_cast(
        0xFFFF_FFFF,
        PrimitiveType::U32,
        TypeTable::U32,
        TypeTable::I32,
        PrimitiveType::I32,
        i64::from(-1_i32) as u64,
    );

    // Same-width reinterpret 64-bit: i64(-1) as u64
    check_env_cast(
        -1_i64 as u64,
        PrimitiveType::I64,
        TypeTable::I64,
        TypeTable::U64,
        PrimitiveType::U64,
        u64::MAX,
    );
}

#[test]
fn cast_through_env_local_applies_target_prim() {
    // Regression: previously the `Cast` fallback path returned the
    // *input's* lattice value verbatim when the operand was a
    // non-literal (e.g. an env-resolved `Local`), so a `u as i32`
    // wrote the raw u32 bits at an i32-typed expression slot. A
    // 0xFFFFFFFF u32 cast to i32 must produce -1, not the positive
    // 4294967295 the buggy path leaked through.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    // env: u = Const(Int{0xFFFFFFFF, U32}) — equivalent to `let u: u32
    // = -1 as u32;`.
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 0xFFFF_FFFF,
            prim: PrimitiveType::U32,
        }),
    );
    let cast_expr = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(local_expr(0, TypeTable::U32)),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        Span::default(),
    );
    // u as i32 must equal -1 (sign-extended bit pattern in u64 form).
    let neg_one_bits = i64::from(-1_i32) as u64;
    assert_eq!(
        interp.reduce_to_lattice(&cast_expr),
        Lattice::Const(Value::Int {
            value: neg_one_bits,
            prim: PrimitiveType::I32,
        }),
    );

    // Crucially the *equality* with the -1 literal must also fold to
    // Const(true) — the original bug surfaced as `(u as i32) == -1`
    // folding to `false` (because the LHS still carried U32 bits at an
    // I32 slot, and the comparator's same-prim eval re-interpreted
    // them as the unsigned 4294967295).
    let lhs = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(local_expr(0, TypeTable::U32)),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        Span::default(),
    );
    let cmp = binary(
        TirBinaryOp::Eq,
        lhs,
        int_lit(neg_one_bits, TypeTable::I32, "-1"),
        TypeTable::BOOL,
    );
    assert_eq!(
        interp.reduce_to_lattice(&cmp),
        Lattice::Const(Value::Bool(true)),
    );
}
