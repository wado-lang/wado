//! Integration tests for [`wado_compiler::tiri`] — the TIR interpreter
//! that powers constant folding (and, eventually, branch / loop reduction).
//!
//! Each test builds a tiny TIR expression, runs it through
//! [`Interpreter::reduce_to_value`], and checks the resulting [`Value`].
//! The focus here is the four arithmetic ops across a handful of
//! representative integer / float types — the goal being to give the
//! interpreter a stable contract to refactor against, not to enumerate
//! every operator.

use wado_compiler::Span;
use wado_compiler::tir::{
    PrimitiveType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};
use wado_compiler::tiri::{Interpreter, Value};

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

fn eval(expr: &TirExpr) -> Option<Value> {
    let table = TypeTable::new();
    Interpreter::new(&table).reduce_to_value(expr)
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
// Non-foldable input — reduce_to_value returns None
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn non_literal_operand_is_unreducible() {
    // A `Unit` expression as the operand stands in for any non-literal —
    // the interpreter has no Value to produce, so it returns None.
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
