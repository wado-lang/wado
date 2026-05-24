//! Integration tests for [`wado_compiler::niri`] — the NIR interpreter
//! that powers constant folding (and, eventually, branch / loop reduction).
//!
//! Each test builds a tiny NIR expression, runs it through
//! [`Interpreter::reduce_to_lattice`], and checks the resulting
//! [`Lattice`]. The focus here is the four arithmetic ops across a
//! handful of representative integer / float types — the goal being to
//! give the interpreter a stable contract to refactor against, not to
//! enumerate every operator.

use std::cell::RefCell;
use std::rc::Rc;

use wado_compiler::Span;
use wado_compiler::hashmap::IndexSet;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::nir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, NirBinaryOp, NirBlock, NirExpr, NirExprKind,
    NirFunction, NirLiteralPattern, NirLocal, NirMatchArm, NirParam, NirPattern, NirStmt,
    NirStmtKind, NirUnaryOp, ReturnAbi,
};
use wado_compiler::niri::{CalleeMap, GlobalEnv, Interpreter, Lattice, Value, is_ctfe_eligible};
use wado_compiler::tir::{EffectRef, PrimitiveType, TypeId, TypeTable};

fn char_lit(c: char) -> NirExpr {
    NirExpr::new(
        NirExprKind::CharLiteral(c),
        TypeTable::CHAR,
        Span::default(),
    )
}

fn cast_expr(inner: NirExpr, target_ty: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::Cast {
            expr: Box::new(inner),
            target_type: target_ty,
        },
        target_ty,
        Span::default(),
    )
}

fn int_lit(value: u64, type_id: TypeId, repr: &str) -> NirExpr {
    NirExpr::new(
        NirExprKind::IntLiteral {
            repr: repr.to_string(),
            value,
        },
        type_id,
        Span::default(),
    )
}

fn float_lit(value: f64, type_id: TypeId, repr: &str) -> NirExpr {
    NirExpr::new(
        NirExprKind::FloatLiteral {
            repr: repr.to_string(),
            value,
        },
        type_id,
        Span::default(),
    )
}

fn bool_lit(value: bool) -> NirExpr {
    NirExpr::new(
        NirExprKind::BoolLiteral(value),
        TypeTable::BOOL,
        Span::default(),
    )
}

fn binary(op: NirBinaryOp, left: NirExpr, right: NirExpr, result_ty: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        },
        result_ty,
        Span::default(),
    )
}

fn unary(op: NirUnaryOp, expr: NirExpr, result_ty: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::Unary {
            op,
            expr: Box::new(expr),
        },
        result_ty,
        Span::default(),
    )
}

fn local_expr(index: u32, type_id: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::Local {
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
fn eval(expr: &NirExpr) -> Option<Value> {
    let table = TypeTable::new();
    Interpreter::new(&table).reduce_to_lattice(expr).as_const()
}

fn expect_int(expr: &NirExpr, expected_value: u64, expected_prim: PrimitiveType) {
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

fn expect_float(expr: &NirExpr, expected_value: f64, expected_prim: PrimitiveType) {
    let v = eval(expr).expect("expected reduction");
    match v {
        Value::Float { value, prim } => {
            assert_eq!(prim, expected_prim, "wrong float width");
            assert_eq!(value, expected_value, "wrong float value");
        }
        other => panic!("expected float, got {other:?}"),
    }
}

fn expect_bool(expr: &NirExpr, expected: bool) {
    assert_eq!(eval(expr), Some(Value::Bool(expected)));
}

// ──────────────────────────────────────────────────────────────────────────────
// i32 — full four-op coverage
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn i32_add() {
    let e = binary(
        NirBinaryOp::Add,
        int_lit(20, TypeTable::I32, "20"),
        int_lit(22, TypeTable::I32, "22"),
        TypeTable::I32,
    );
    expect_int(&e, 42, PrimitiveType::I32);
}

#[test]
fn i32_sub_negative_result() {
    let e = binary(
        NirBinaryOp::Sub,
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
        NirBinaryOp::Mul,
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
        NirBinaryOp::Div,
        int_lit((-7_i32) as u64, TypeTable::I32, "-7"),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    );
    expect_int(&e, (-3_i32) as u64, PrimitiveType::I32);
}

#[test]
fn i32_div_by_zero_is_unreducible() {
    let e = binary(
        NirBinaryOp::Div,
        int_lit(42, TypeTable::I32, "42"),
        int_lit(0, TypeTable::I32, "0"),
        TypeTable::I32,
    );
    assert_eq!(eval(&e), None, "div-by-zero must preserve the runtime trap");
}

#[test]
fn i32_div_min_by_neg_one_is_unreducible() {
    let e = binary(
        NirBinaryOp::Div,
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
        NirBinaryOp::Add,
        int_lit(1_000_000_000_000, TypeTable::I64, "1000000000000"),
        int_lit(1, TypeTable::I64, "1"),
        TypeTable::I64,
    );
    expect_int(&add, 1_000_000_000_001, PrimitiveType::I64);

    let mul = binary(
        NirBinaryOp::Mul,
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
        NirBinaryOp::Add,
        int_lit(255, TypeTable::U8, "255"),
        int_lit(1, TypeTable::U8, "1"),
        TypeTable::U8,
    );
    expect_int(&e, 0, PrimitiveType::U8);
}

#[test]
fn u8_sub_wraps() {
    let e = binary(
        NirBinaryOp::Sub,
        int_lit(0, TypeTable::U8, "0"),
        int_lit(1, TypeTable::U8, "1"),
        TypeTable::U8,
    );
    expect_int(&e, 255, PrimitiveType::U8);
}

#[test]
fn u8_div_unsigned() {
    let e = binary(
        NirBinaryOp::Div,
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
        NirBinaryOp::Mod,
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
        NirBinaryOp::Add,
        float_lit(1.5, TypeTable::F64, "1.5"),
        float_lit(2.5, TypeTable::F64, "2.5"),
        TypeTable::F64,
    );
    expect_float(&e, 4.0, PrimitiveType::F64);
}

#[test]
fn f64_sub() {
    let e = binary(
        NirBinaryOp::Sub,
        float_lit(10.0, TypeTable::F64, "10.0"),
        float_lit(3.5, TypeTable::F64, "3.5"),
        TypeTable::F64,
    );
    expect_float(&e, 6.5, PrimitiveType::F64);
}

#[test]
fn f64_mul() {
    let e = binary(
        NirBinaryOp::Mul,
        float_lit(3.0, TypeTable::F64, "3.0"),
        float_lit(2.0, TypeTable::F64, "2.0"),
        TypeTable::F64,
    );
    expect_float(&e, 6.0, PrimitiveType::F64);
}

#[test]
fn f64_div() {
    let e = binary(
        NirBinaryOp::Div,
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
        NirBinaryOp::Div,
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
        NirBinaryOp::Div,
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
        NirBinaryOp::Div,
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
        NirUnaryOp::Neg,
        int_lit(42, TypeTable::I32, "42"),
        TypeTable::I32,
    );
    expect_int(&e, (-42_i32) as u64, PrimitiveType::I32);
}

#[test]
fn u32_neg_is_unreducible() {
    // Negation is undefined on unsigned ints — must not fold.
    let e = unary(
        NirUnaryOp::Neg,
        int_lit(42, TypeTable::U32, "42"),
        TypeTable::U32,
    );
    assert_eq!(eval(&e), None);
}

#[test]
fn f64_neg_zero() {
    let e = unary(
        NirUnaryOp::Neg,
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
    let e = unary(NirUnaryOp::Not, bool_lit(true), TypeTable::BOOL);
    expect_bool(&e, false);
}

// ──────────────────────────────────────────────────────────────────────────────
// Nested expressions — interpreter recurses through children
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn nested_int_arithmetic() {
    // (2 + 3) * (10 - 4) == 30
    let lhs = binary(
        NirBinaryOp::Add,
        int_lit(2, TypeTable::I32, "2"),
        int_lit(3, TypeTable::I32, "3"),
        TypeTable::I32,
    );
    let rhs = binary(
        NirBinaryOp::Sub,
        int_lit(10, TypeTable::I32, "10"),
        int_lit(4, TypeTable::I32, "4"),
        TypeTable::I32,
    );
    let e = binary(NirBinaryOp::Mul, lhs, rhs, TypeTable::I32);
    expect_int(&e, 30, PrimitiveType::I32);
}

#[test]
fn comparison_yields_bool() {
    let e = binary(
        NirBinaryOp::Lt,
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
        NirBinaryOp::Add,
        int_lit(1, TypeTable::I32, "1"),
        NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, Span::default()),
        TypeTable::I32,
    );
    assert_eq!(eval(&e), None);
}

// ──────────────────────────────────────────────────────────────────────────────
// `reduce` returns a NirExpr — repr preservation and shape contracts
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
        NirExprKind::IntLiteral { repr, value } => {
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
        NirBinaryOp::Add,
        int_lit(20, TypeTable::I32, "20"),
        int_lit(22, TypeTable::I32, "22"),
        TypeTable::I32,
    );
    let reduced = Interpreter::new(&table).reduce(&e);
    match reduced.kind {
        NirExprKind::IntLiteral { value, .. } => assert_eq!(value, 42),
        other => panic!("expected IntLiteral after fold, got {other:?}"),
    }
}

#[test]
fn reduce_short_circuits_or_false() {
    // `false || X` reduces to `X` even when `X` is non-constant.
    let table = TypeTable::new();
    let lhs = bool_lit(false);
    let rhs = NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, Span::default());
    let e = binary(NirBinaryOp::Or, lhs, rhs, TypeTable::BOOL);
    let reduced = Interpreter::new(&table).reduce(&e);
    assert!(
        matches!(reduced.kind, NirExprKind::Unit),
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
    let e = NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, Span::default());
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
        NirBinaryOp::Div,
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
// Local-variable env: bind_local, invalidate_local, function reset
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
        NirBinaryOp::Add,
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
        NirBinaryOp::Add,
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
    // produce malformed NIR).
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
        matches!(reduced.kind, NirExprKind::Local { index: 0, .. }),
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
        NirBinaryOp::Add,
        local_expr(0, TypeTable::I32),
        local_expr(1, TypeTable::I32),
        TypeTable::I32,
    );
    let e = binary(
        NirBinaryOp::Mul,
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
/// to `Const(Int{src_value, src_prim})`. Exercises the env-resolved
/// cast path that a previous regression silently corrupted.
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
    let cast_expr = NirExpr::new(
        NirExprKind::Cast {
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
    let cast_expr = NirExpr::new(
        NirExprKind::Cast {
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
    let lhs = NirExpr::new(
        NirExprKind::Cast {
            expr: Box::new(local_expr(0, TypeTable::U32)),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        Span::default(),
    );
    let cmp = binary(
        NirBinaryOp::Eq,
        lhs,
        int_lit(neg_one_bits, TypeTable::I32, "-1"),
        TypeTable::BOOL,
    );
    assert_eq!(
        interp.reduce_to_lattice(&cmp),
        Lattice::Const(Value::Bool(true)),
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// `Lattice::join` and `if`-expression reduction
// ──────────────────────────────────────────────────────────────────────────────

fn block_with_tail_expr(e: NirExpr) -> NirBlock {
    NirBlock::new(
        vec![NirStmt::new(NirStmtKind::Expr(e), Span::default())],
        Span::default(),
    )
}

fn if_expr(
    condition: NirExpr,
    then_branch: NirBlock,
    else_branch: Option<NirBlock>,
    type_id: TypeId,
) -> NirExpr {
    NirExpr::new(
        NirExprKind::If {
            condition: Box::new(condition),
            then_branch,
            else_branch,
        },
        type_id,
        Span::default(),
    )
}

#[test]
fn lattice_join_idempotent_on_equal_consts() {
    let v = Lattice::Const(Value::Int {
        value: 7,
        prim: PrimitiveType::I32,
    });
    assert_eq!(v.join(v), v);
}

#[test]
fn lattice_join_unequal_consts_is_nonconst() {
    let a = Lattice::Const(Value::Int {
        value: 1,
        prim: PrimitiveType::I32,
    });
    let b = Lattice::Const(Value::Int {
        value: 2,
        prim: PrimitiveType::I32,
    });
    assert_eq!(a.join(b), Lattice::NonConst);
}

#[test]
fn lattice_join_unevaluated_is_identity() {
    // Unevaluated is the SCCP infeasible-edge value: joining with it
    // returns the other operand. The non-constant-condition path that
    // wants Top semantics for unknown arms must promote Unevaluated →
    // NonConst *before* invoking `join`; that promotion happens inside
    // `expr_to_lattice` for `If`, not inside `join` itself.
    let c = Lattice::Const(Value::Int {
        value: 42,
        prim: PrimitiveType::I64,
    });
    assert_eq!(Lattice::Unevaluated.join(c), c);
    assert_eq!(c.join(Lattice::Unevaluated), c);
    assert_eq!(
        Lattice::Unevaluated.join(Lattice::Unevaluated),
        Lattice::Unevaluated
    );
}

#[test]
fn lattice_join_nonconst_is_absorbing() {
    let c = Lattice::Const(Value::Bool(true));
    assert_eq!(Lattice::NonConst.join(c), Lattice::NonConst);
    assert_eq!(c.join(Lattice::NonConst), Lattice::NonConst);
    assert_eq!(
        Lattice::NonConst.join(Lattice::Unevaluated),
        Lattice::NonConst
    );
}

#[test]
fn lattice_join_is_commutative_and_associative() {
    let a = Lattice::Const(Value::Int {
        value: 3,
        prim: PrimitiveType::I32,
    });
    let b = Lattice::Const(Value::Int {
        value: 4,
        prim: PrimitiveType::I32,
    });
    let c = Lattice::NonConst;
    assert_eq!(a.join(b), b.join(a));
    assert_eq!(a.join(b).join(c), a.join(b.join(c)));
    assert_eq!(b.join(c).join(a), c.join(a).join(b));
}

#[test]
fn if_const_true_picks_then_arm() {
    let table = TypeTable::new();
    let expr = if_expr(
        bool_lit(true),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        Some(block_with_tail_expr(int_lit(20, TypeTable::I32, "20"))),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 10,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn if_const_false_picks_else_arm() {
    let table = TypeTable::new();
    let expr = if_expr(
        bool_lit(false),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        Some(block_with_tail_expr(int_lit(20, TypeTable::I32, "20"))),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 20,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn if_const_false_no_else_yields_unit() {
    // `if false { … }` (no else) has type Unit; niri models Unit as
    // having no representable Const — so the lattice is Unevaluated.
    // Crucially, the Unevaluated *result* must not poison a surrounding
    // join with a Const peer (covered by the non-const-condition
    // expr_to_lattice promotion); here we only verify the bare lookup.
    let table = TypeTable::new();
    let expr = if_expr(
        bool_lit(false),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        None,
        TypeTable::UNIT,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Unevaluated,
    );
}

#[test]
fn if_const_true_unreachable_else_does_not_contaminate() {
    // SCCP "infeasible edge" treatment: when the condition is provably
    // true, the else arm is not consulted at all. Even if its tail is
    // a different lattice value (here `99`, which would otherwise
    // disagree with `42` and force NonConst under a join), the chosen
    // arm's value flows out unchanged.
    let table = TypeTable::new();
    let expr = if_expr(
        bool_lit(true),
        block_with_tail_expr(int_lit(42, TypeTable::I32, "42")),
        Some(block_with_tail_expr(int_lit(99, TypeTable::I32, "99"))),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 42,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn if_nonconst_cond_with_equal_arm_consts_folds() {
    // Both arms reduce to the same Const(5). With a speculatable
    // condition (a Local), the if collapses to `5`.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    // Local 0 is a bool, unbound in env → `Unevaluated`. is_speculatable
    // accepts Local, so the both-arms-equal collapse is allowed to fire.
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(int_lit(5, TypeTable::I32, "5")),
        Some(block_with_tail_expr(int_lit(5, TypeTable::I32, "5"))),
        TypeTable::I32,
    );
    assert_eq!(
        interp.reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn if_nonconst_cond_with_unequal_arm_consts_is_nonconst() {
    // Different Const arms under a non-constant condition: the merged
    // lattice is `NonConst`, and the `if` is not rewritten.
    let table = TypeTable::new();
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(int_lit(1, TypeTable::I32, "1")),
        Some(block_with_tail_expr(int_lit(2, TypeTable::I32, "2"))),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn if_nonconst_cond_with_unevaluated_arm_does_not_fold() {
    // Regression: under a non-constant condition, an arm whose tail is
    // a non-literal (here `Local 1`, lattice = Unevaluated since not in
    // env) MUST NOT let the surrounding `if` collapse to the *other*
    // arm's Const value. The fix promotes Unevaluated → NonConst before
    // joining, so the merged lattice is NonConst (not the else arm's
    // Const(0)).
    let table = TypeTable::new();
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(local_expr(1, TypeTable::I32)),
        Some(block_with_tail_expr(int_lit(0, TypeTable::I32, "0"))),
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn reduce_local_rewrites_const_true_if_to_block() {
    // The visitor-driven path: `reduce_local` rewrites the `If` in
    // place to a `Block` of the chosen branch. This is the rewrite that
    // subsumes the `if true` case from the legacy `const_branch_prune`.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        bool_lit(true),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        Some(block_with_tail_expr(int_lit(20, TypeTable::I32, "20"))),
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Block(b) = &expr.kind else {
        panic!("expected Block, got {:?}", expr.kind);
    };
    assert_eq!(b.stmts.len(), 1);
    let NirStmtKind::Expr(tail) = &b.stmts[0].kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = tail.kind else {
        panic!("expected IntLiteral tail");
    };
    assert_eq!(value, 10);
}

#[test]
fn reduce_local_rewrites_const_false_if_no_else_to_unit() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        bool_lit(false),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        None,
        TypeTable::UNIT,
    );
    assert!(interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Unit));
}

#[test]
fn reduce_local_collapses_equal_arm_if_to_literal() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(int_lit(7, TypeTable::I32, "7")),
        Some(block_with_tail_expr(int_lit(7, TypeTable::I32, "7"))),
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral, got {:?}", expr.kind);
    };
    assert_eq!(value, 7);
}

#[test]
fn reduce_local_rewrites_if_true_false_to_cond() {
    // `if cond { true } else { false }` → `cond`. Common shape produced
    // by `match X { V => true, _ => false }` → branch lowering and by
    // user-written explicit bool selection. The condition is preserved
    // unchanged so the speculatable-ness gate the both-arms-equal rule
    // requires does not apply here.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(bool_lit(false))),
        TypeTable::BOOL,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Local { index, .. } = expr.kind else {
        panic!("expected Local (the original condition), got {:?}", expr.kind);
    };
    assert_eq!(index, 0);
    assert_eq!(expr.type_id, TypeTable::BOOL);
}

#[test]
fn reduce_local_rewrites_if_false_true_to_not_cond() {
    // `if cond { false } else { true }` → `!cond`. The Unary::Not wrap
    // preserves the same observable behaviour as the original `if` —
    // truth and falsity are swapped, evaluation order is identical
    // (cond is evaluated, then negated).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(false)),
        Some(block_with_tail_expr(bool_lit(true))),
        TypeTable::BOOL,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Unary { op, expr: inner } = &expr.kind else {
        panic!("expected Unary::Not, got {:?}", expr.kind);
    };
    assert!(matches!(op, NirUnaryOp::Not));
    let NirExprKind::Local { index, .. } = inner.kind else {
        panic!("expected Local inside Unary::Not, got {:?}", inner.kind);
    };
    assert_eq!(index, 0);
    assert_eq!(expr.type_id, TypeTable::BOOL);
}

#[test]
fn reduce_local_rewrites_if_true_false_with_non_speculatable_cond() {
    // The cond-preservation rule does NOT require `is_speculatable(cond)`
    // because the rewrite keeps the condition's evaluation intact. Use a
    // `Match` expression as the cond (not speculatable per niri's check)
    // and verify the rule still fires.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let impure_cond = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(0), bool_lit(true)),
            arm(NirPattern::Wildcard, bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let mut expr = if_expr(
        impure_cond,
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(bool_lit(false))),
        TypeTable::BOOL,
    );
    assert!(interp.reduce_local(&mut expr));
    // After rewrite, the if is replaced by the (still-non-speculatable)
    // cond expression itself — the Match.
    assert!(
        matches!(expr.kind, NirExprKind::Match { .. }),
        "expected the original Match condition to survive as the result, got {:?}",
        expr.kind
    );
}

#[test]
fn reduce_local_leaves_if_mixed_bool_int_arms_alone() {
    // Defensive: when arms have different types (bool then-arm, int
    // else-arm) the bool-arms rule must not fire. The (Bool, Bool)
    // tuple pattern in the rule guards against this.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(int_lit(0, TypeTable::I32, "0"))),
        // Type intentionally mismatched between if-expr and arms — a
        // resolver-level invariant, but we want the rule to stay silent
        // regardless.
        TypeTable::BOOL,
    );
    let before = format!("{:?}", expr.kind);
    assert!(!interp.reduce_local(&mut expr));
    assert_eq!(format!("{:?}", expr.kind), before);
}

#[test]
fn reduce_local_block_splices_const_true_if_stmt() {
    // Stmt-form `if true { stmts… }` → splice stmts into the parent.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let inner_stmt = NirStmt::new(
        NirStmtKind::Expr(int_lit(99, TypeTable::I32, "99")),
        Span::default(),
    );
    let if_stmt = NirStmt::new(
        NirStmtKind::If {
            condition: bool_lit(true),
            then_block: NirBlock::new(vec![inner_stmt], Span::default()),
            else_block: Some(NirBlock::new(
                vec![NirStmt::new(
                    NirStmtKind::Expr(int_lit(0, TypeTable::I32, "0")),
                    Span::default(),
                )],
                Span::default(),
            )),
        },
        Span::default(),
    );
    let mut block = NirBlock::new(vec![if_stmt], Span::default());
    assert!(interp.reduce_local_block(&mut block));
    assert_eq!(block.stmts.len(), 1);
    let NirStmtKind::Expr(e) = &block.stmts[0].kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = e.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 99);
}

#[test]
fn reduce_local_block_drops_const_false_if_stmt_without_else() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let if_stmt = NirStmt::new(
        NirStmtKind::If {
            condition: bool_lit(false),
            then_block: NirBlock::new(
                vec![NirStmt::new(
                    NirStmtKind::Expr(int_lit(99, TypeTable::I32, "99")),
                    Span::default(),
                )],
                Span::default(),
            ),
            else_block: None,
        },
        Span::default(),
    );
    let mut block = NirBlock::new(vec![if_stmt], Span::default());
    assert!(interp.reduce_local_block(&mut block));
    assert!(block.stmts.is_empty());
}

#[test]
fn reduce_local_block_leaves_nonconst_if_alone() {
    // Stmt-form `if cond { … }` with a non-literal condition is left
    // structurally intact — `reduce_local_block` must not touch it.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let if_stmt = NirStmt::new(
        NirStmtKind::If {
            condition: local_expr(0, TypeTable::BOOL),
            then_block: NirBlock::new(
                vec![NirStmt::new(
                    NirStmtKind::Expr(int_lit(99, TypeTable::I32, "99")),
                    Span::default(),
                )],
                Span::default(),
            ),
            else_block: None,
        },
        Span::default(),
    );
    let mut block = NirBlock::new(vec![if_stmt], Span::default());
    assert!(!interp.reduce_local_block(&mut block));
    assert_eq!(block.stmts.len(), 1);
    assert!(matches!(block.stmts[0].kind, NirStmtKind::If { .. }));
}

// ──────────────────────────────────────────────────────────────────────────────
// Cast — bool / char / int ↔ float
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn cast_bool_true_to_i32_is_one() {
    let e = cast_expr(bool_lit(true), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 1,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_bool_false_to_i32_is_zero() {
    let e = cast_expr(bool_lit(false), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 0,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_bool_to_i64_zero_extends() {
    let e = cast_expr(bool_lit(true), TypeTable::I64);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 1,
            prim: PrimitiveType::I64
        })
    );
}

#[test]
fn cast_bool_to_u8_is_one() {
    let e = cast_expr(bool_lit(true), TypeTable::U8);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 1,
            prim: PrimitiveType::U8
        })
    );
}

#[test]
fn cast_bool_to_f64_is_one_point_zero() {
    let e = cast_expr(bool_lit(true), TypeTable::F64);
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 1.0,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_bool_to_f32_is_zero_point_zero() {
    let e = cast_expr(bool_lit(false), TypeTable::F32);
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 0.0,
            prim: PrimitiveType::F32
        })
    );
}

#[test]
fn cast_char_to_i32_is_codepoint() {
    let e = cast_expr(char_lit('A'), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 65,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_char_emoji_to_i32_is_codepoint() {
    let e = cast_expr(char_lit('😀'), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 0x1F600,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_char_to_u32_is_codepoint() {
    let e = cast_expr(char_lit('A'), TypeTable::U32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 65,
            prim: PrimitiveType::U32
        })
    );
}

#[test]
fn cast_char_to_u8_truncates() {
    // U+0141 (Ł) — codepoint 0x141; low byte is 0x41.
    let e = cast_expr(char_lit('Ł'), TypeTable::U8);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 0x41,
            prim: PrimitiveType::U8
        })
    );
}

#[test]
fn cast_u8_to_char_succeeds() {
    let e = cast_expr(int_lit(65, TypeTable::U8, "65"), TypeTable::CHAR);
    assert_eq!(eval(&e), Some(Value::Char('A')));
}

#[test]
fn cast_u8_max_to_char_is_y_with_diaeresis() {
    let e = cast_expr(int_lit(255, TypeTable::U8, "255"), TypeTable::CHAR);
    assert_eq!(eval(&e), Some(Value::Char('\u{FF}')));
}

#[test]
fn cast_i32_to_f64_signed_convert() {
    let e = cast_expr(
        int_lit((-42_i32) as u64, TypeTable::I32, "-42"),
        TypeTable::F64,
    );
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: -42.0,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_u32_large_to_f64_unsigned_convert() {
    // 3_000_000_000 is > i32::MAX so a signed conversion would yield a
    // negative number — this checks the unsigned path.
    let e = cast_expr(
        int_lit(3_000_000_000, TypeTable::U32, "3000000000"),
        TypeTable::F64,
    );
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 3_000_000_000.0,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_u64_huge_to_f64_unsigned_convert() {
    let e = cast_expr(
        int_lit(
            10_000_000_000_000_000_000,
            TypeTable::U64,
            "10000000000000000000",
        ),
        TypeTable::F64,
    );
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 10_000_000_000_000_000_000.0,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_i32_to_f32_signed_convert() {
    let e = cast_expr(int_lit(42, TypeTable::I32, "42"), TypeTable::F32);
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 42.0,
            prim: PrimitiveType::F32
        })
    );
}

#[test]
fn cast_i8_negative_to_f64_preserves_sign() {
    // -5 as i8 has bit pattern 0xFB (sign-extended to 0xFFFF_FFFF_FFFF_FFFB).
    let e = cast_expr(
        int_lit(i64::from(-5_i8) as u64, TypeTable::I8, "-5"),
        TypeTable::F64,
    );
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: -5.0,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_f64_to_i32_truncates_toward_zero() {
    let e = cast_expr(float_lit(2.7, TypeTable::F64, "2.7"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 2,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_f64_negative_to_i32_truncates_toward_zero() {
    let e = cast_expr(float_lit(-7.9, TypeTable::F64, "-7.9"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: (-7_i32) as u64,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_f64_to_u32_unsigned_trunc() {
    let e = cast_expr(
        float_lit(3_000_000_000.0, TypeTable::F64, "3000000000.0"),
        TypeTable::U32,
    );
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 3_000_000_000,
            prim: PrimitiveType::U32
        })
    );
}

#[test]
fn cast_f64_nan_to_i32_is_zero() {
    // Wasm `i32.trunc_sat_f64_s` says NaN → 0. Rust's `as` matches.
    let e = cast_expr(float_lit(f64::NAN, TypeTable::F64, "nan"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 0,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_f64_huge_to_i32_saturates_to_max() {
    let e = cast_expr(float_lit(1e30, TypeTable::F64, "1e30"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: u64::from(i32::MAX as u32),
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_f64_neg_huge_to_i32_saturates_to_min() {
    let e = cast_expr(float_lit(-1e30, TypeTable::F64, "-1e30"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: i64::from(i32::MIN) as u64,
            prim: PrimitiveType::I32,
        })
    );
}

#[test]
fn cast_f32_to_f64_promotes_exactly() {
    let e = cast_expr(float_lit(1.5, TypeTable::F32, "1.5"), TypeTable::F64);
    assert_eq!(
        eval(&e),
        Some(Value::Float {
            value: 1.5,
            prim: PrimitiveType::F64
        })
    );
}

#[test]
fn cast_f64_to_f32_rounds() {
    // 0.1 (f64) is not exactly representable in f32, so the cast
    // through f32 produces a different bit pattern than the original
    // f64 — that's the rounding step we want to observe.
    let e = cast_expr(float_lit(0.1, TypeTable::F64, "0.1"), TypeTable::F32);
    let v = eval(&e).expect("expected reduction");
    match v {
        Value::Float { value, prim } => {
            assert_eq!(prim, PrimitiveType::F32);
            assert_eq!(
                value,
                f64::from(0.1_f32),
                "0.1 rounded to f32, then widened"
            );
            assert_ne!(value, 0.1_f64, "rounding step must change the bits");
        }
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn cast_f32_to_i32_uses_f32_precision_for_truncation() {
    let e = cast_expr(float_lit(1000.0, TypeTable::F32, "1000.0"), TypeTable::I32);
    assert_eq!(
        eval(&e),
        Some(Value::Int {
            value: 1000,
            prim: PrimitiveType::I32
        })
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Bool ordering / Char comparisons
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn bool_lt_false_lt_true_is_true() {
    let e = binary(
        NirBinaryOp::Lt,
        bool_lit(false),
        bool_lit(true),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn bool_lt_true_lt_false_is_false() {
    let e = binary(
        NirBinaryOp::Lt,
        bool_lit(true),
        bool_lit(false),
        TypeTable::BOOL,
    );
    expect_bool(&e, false);
}

#[test]
fn bool_lt_eq_reflexive() {
    let e = binary(
        NirBinaryOp::LtEq,
        bool_lit(true),
        bool_lit(true),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn bool_gt() {
    let e = binary(
        NirBinaryOp::Gt,
        bool_lit(true),
        bool_lit(false),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn bool_gt_eq_reflexive() {
    let e = binary(
        NirBinaryOp::GtEq,
        bool_lit(false),
        bool_lit(false),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_eq_equal_chars_is_true() {
    let e = binary(
        NirBinaryOp::Eq,
        char_lit('A'),
        char_lit('A'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_eq_different_chars_is_false() {
    let e = binary(
        NirBinaryOp::Eq,
        char_lit('A'),
        char_lit('B'),
        TypeTable::BOOL,
    );
    expect_bool(&e, false);
}

#[test]
fn char_not_eq() {
    let e = binary(
        NirBinaryOp::NotEq,
        char_lit('A'),
        char_lit('B'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_lt_is_codepoint_order() {
    let e = binary(
        NirBinaryOp::Lt,
        char_lit('A'),
        char_lit('B'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_gt() {
    let e = binary(
        NirBinaryOp::Gt,
        char_lit('z'),
        char_lit('a'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_lt_eq_reflexive() {
    let e = binary(
        NirBinaryOp::LtEq,
        char_lit('m'),
        char_lit('m'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

#[test]
fn char_unicode_lt() {
    let e = binary(
        NirBinaryOp::Lt,
        char_lit('a'),
        char_lit('日'),
        TypeTable::BOOL,
    );
    expect_bool(&e, true);
}

// ──────────────────────────────────────────────────────────────────────────────
// CharLiteral lattice + arithmetic-on-char rejection
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn char_literal_reduces_to_const() {
    assert_eq!(eval(&char_lit('A')), Some(Value::Char('A')));
}

#[test]
fn char_arithmetic_is_unreducible() {
    // char does not implement Add — the resolver rejects it, but if a
    // synthesized node ever reaches niri it must not fold.
    let e = binary(
        NirBinaryOp::Add,
        char_lit('A'),
        char_lit('B'),
        TypeTable::CHAR,
    );
    assert_eq!(eval(&e), None);
}

// ──────────────────────────────────────────────────────────────────────────────
// Cast through env-resolved Local for non-int sources
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn cast_bool_to_int_through_env_local() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(0, Lattice::Const(Value::Bool(true)));
    let e = cast_expr(local_expr(0, TypeTable::BOOL), TypeTable::I32);
    assert_eq!(
        interp.reduce_to_lattice(&e),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_char_to_int_through_env_local() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(0, Lattice::Const(Value::Char('A')));
    let e = cast_expr(local_expr(0, TypeTable::CHAR), TypeTable::I32);
    assert_eq!(
        interp.reduce_to_lattice(&e),
        Lattice::Const(Value::Int {
            value: 65,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn cast_int_to_float_through_env_local() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 42,
            prim: PrimitiveType::I32,
        }),
    );
    let e = cast_expr(local_expr(0, TypeTable::I32), TypeTable::F64);
    assert_eq!(
        interp.reduce_to_lattice(&e),
        Lattice::Const(Value::Float {
            value: 42.0,
            prim: PrimitiveType::F64
        })
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// `match` expression reduction (payload-free patterns)
// ──────────────────────────────────────────────────────────────────────────────

fn match_expr(scrutinee: NirExpr, arms: Vec<NirMatchArm>, type_id: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::Match {
            expr: Box::new(scrutinee),
            arms,
        },
        type_id,
        Span::default(),
    )
}

fn arm(pattern: NirPattern, body: NirExpr) -> NirMatchArm {
    NirMatchArm {
        pattern,
        guard: None,
        body,
        span: Span::default(),
    }
}

fn arm_with_guard(pattern: NirPattern, guard: NirExpr, body: NirExpr) -> NirMatchArm {
    NirMatchArm {
        pattern,
        guard: Some(guard),
        body,
        span: Span::default(),
    }
}

fn lit_pat_i128(value: i128) -> NirPattern {
    NirPattern::Literal(NirLiteralPattern::I128(value))
}

fn lit_pat_u128(value: u128) -> NirPattern {
    NirPattern::Literal(NirLiteralPattern::U128(value))
}

fn lit_pat_bool(value: bool) -> NirPattern {
    NirPattern::Literal(NirLiteralPattern::Bool(value))
}

fn lit_pat_char(value: char) -> NirPattern {
    NirPattern::Literal(NirLiteralPattern::Char(value))
}

fn range_pat(start: i128, end: i128, inclusive: bool, is_unsigned: bool) -> NirPattern {
    NirPattern::Range {
        start,
        end,
        inclusive,
        is_unsigned,
    }
}

#[test]
fn match_const_int_picks_matching_arm() {
    // `match 2 { 1 => 10, 2 => 20, _ => 30 }` should reduce to `Const(20)`.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(2, TypeTable::I32, "2"),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(lit_pat_i128(2), int_lit(20, TypeTable::I32, "20")),
            arm(NirPattern::Wildcard, int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 20,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_falls_through_to_wildcard() {
    // No literal arm matches; the wildcard absorbs it.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(99, TypeTable::I32, "99"),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(NirPattern::Wildcard, int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 30,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_bool_picks_arm() {
    // `match true { true => 1, false => 0 }` → 1.
    let table = TypeTable::new();
    let expr = match_expr(
        bool_lit(true),
        vec![
            arm(lit_pat_bool(true), int_lit(1, TypeTable::I32, "1")),
            arm(lit_pat_bool(false), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_char_or_pattern_matches_alt() {
    // `match 'b' { 'a' | 'b' | 'c' => 1, _ => 0 }` → 1.
    let table = TypeTable::new();
    let expr = match_expr(
        char_lit('b'),
        vec![
            arm(
                NirPattern::Or(vec![
                    lit_pat_char('a'),
                    lit_pat_char('b'),
                    lit_pat_char('c'),
                ]),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_char_or_pattern_misses_all_alts() {
    // `match 'z' { 'a' | 'b' => 1, _ => 0 }` → 0.
    let table = TypeTable::new();
    let expr = match_expr(
        char_lit('z'),
        vec![
            arm(
                NirPattern::Or(vec![lit_pat_char('a'), lit_pat_char('b')]),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_inclusive_range_at_upper_bound() {
    // `0..=10` includes 10. `match 10 { 0..=10 => 1, _ => 0 }` → 1.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(10, TypeTable::I32, "10"),
        vec![
            arm(
                range_pat(0, 10, true, false),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_exclusive_range_excludes_upper_bound() {
    // `0..<10` excludes 10. `match 10 { 0..<10 => 1, _ => 0 }` → 0.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(10, TypeTable::I32, "10"),
        vec![
            arm(
                range_pat(0, 10, false, false),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_signed_negative_in_range() {
    // Signed range covering negatives: `-5..=5` matches -3.
    let table = TypeTable::new();
    let neg_three_bits = i64::from(-3_i32) as u64;
    let expr = match_expr(
        int_lit(neg_three_bits, TypeTable::I32, "-3"),
        vec![
            arm(
                range_pat(-5, 5, true, false),
                int_lit(7, TypeTable::I32, "7"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_u8_unsigned_range() {
    // `match 200u8 { 0..=255 => 1, _ => 0 }` → 1, with is_unsigned=true.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(200, TypeTable::U8, "200"),
        vec![
            arm(
                range_pat(0, 255, true, true),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_u128_literal_pattern() {
    // u128 literal pattern matches a u128 value.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(42, TypeTable::U64, "42"),
        vec![
            arm(lit_pat_u128(42), int_lit(1, TypeTable::I32, "1")),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_unreachable_arm_does_not_contaminate() {
    // SCCP infeasible-edge analogue: when the scrutinee is constant and
    // the matching arm is identified, later arms (with values that would
    // disagree under a join) MUST NOT contribute to the result. The
    // engine simply returns the chosen arm's value.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![
            arm(lit_pat_i128(1), int_lit(5, TypeTable::I32, "5")),
            // This arm "would" reduce to NonConst because the inner
            // expression isn't foldable; under feasible-edge semantics
            // it is dropped from the lattice computation.
            arm(
                NirPattern::Wildcard,
                local_expr(0, TypeTable::I32), // unbound → Unevaluated
            ),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 5,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_nonconst_scrut_all_arms_equal_collapses() {
    // `match x { 1 => 7, 2 => 7, _ => 7 }` with non-constant `x`
    // (speculatable Local) collapses to `7`.
    let table = TypeTable::new();
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(lit_pat_i128(2), int_lit(7, TypeTable::I32, "7")),
            arm(NirPattern::Wildcard, int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_nonconst_scrut_unequal_arms_is_nonconst() {
    let table = TypeTable::new();
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(1, TypeTable::I32, "1")),
            arm(NirPattern::Wildcard, int_lit(2, TypeTable::I32, "2")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_nonconst_scrut_with_unevaluated_arm_does_not_fold() {
    // Regression mirroring the `if` Unevaluated-arm test: under a
    // non-constant scrutinee, an arm whose body is structurally a
    // `Local` (Unevaluated) must promote to NonConst before the join
    // — so the surrounding match cannot collapse to the *other* arm's
    // Const.
    let table = TypeTable::new();
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(NirPattern::Wildcard, local_expr(1, TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_with_guard_under_const_scrut_does_not_rewrite() {
    // Guards inspect bindings niri does not model; the engine cannot
    // commit to the guarded arm even when its pattern would
    // otherwise definitely match. The lattice can still be `Const(7)`
    // (the only candidate body produces 7; the trap-on-guard-failure
    // case is unreachable, hence Bottom in SCCP), but the match
    // expression must NOT be rewritten — that would erase the trap.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm_with_guard(
            lit_pat_i128(1),
            local_expr(0, TypeTable::BOOL),
            int_lit(7, TypeTable::I32, "7"),
        )],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn match_guarded_arm_blocks_rewrite_to_later_definite_arm() {
    // The guarded arm could fire if the guard succeeds, so the
    // engine cannot skip it and pick the later wildcard arm. The
    // match must be left structurally intact.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![
            arm_with_guard(
                lit_pat_i128(1),
                local_expr(0, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
            arm(NirPattern::Wildcard, int_lit(8, TypeTable::I32, "8")),
        ],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn match_guarded_arm_two_distinct_arm_bodies_is_nonconst() {
    // Lattice-level check: when a guarded arm and a later definite
    // arm produce different Const bodies, the merged lattice goes to
    // NonConst (the value depends on whether the guard fires).
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![
            arm_with_guard(
                lit_pat_i128(1),
                local_expr(0, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
            arm(NirPattern::Wildcard, int_lit(8, TypeTable::I32, "8")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_const_scrut_unsupported_pattern_does_not_rewrite() {
    // A pattern the engine doesn't model (Tuple here, Phase A scope)
    // is treated as Unknown — the rewrite step must bail since we
    // cannot prove the arm fires. The match expression must remain
    // structurally intact (so its trap-on-no-match behaviour is
    // preserved at runtime).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm(
            NirPattern::Tuple(vec![], false),
            int_lit(99, TypeTable::I32, "99"),
        )],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_rewrites_const_match_to_arm_body_block() {
    // The visitor-driven path: `reduce_local` rewrites a constant-scrut
    // `Match` in place to a `Block` containing the chosen arm's body
    // expression as a single tail statement.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(2, TypeTable::I32, "2"),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(lit_pat_i128(2), int_lit(20, TypeTable::I32, "20")),
            arm(NirPattern::Wildcard, int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Block(b) = &expr.kind else {
        panic!("expected Block, got {:?}", expr.kind);
    };
    assert_eq!(b.stmts.len(), 1);
    let NirStmtKind::Expr(tail) = &b.stmts[0].kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = tail.kind else {
        panic!("expected IntLiteral tail");
    };
    assert_eq!(value, 20);
}

// ──────────────────────────────────────────────────────────────────────────────
// `match X { CasePattern => true, _ => false }` collapse (A2)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn reduce_local_collapses_enum_match_true_false_to_eq() {
    // `match X { Enum::Case => true, _ => false }` → `X == Enum::Case`.
    // The enum_type is opaque to niri (passed through to the synthesised
    // EnumConstruct + Binary::Eq), so the test uses a stand-in TypeId
    // for the enum.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let enum_ty = TypeTable::I32; // stand-in; not inspected by the rule
    let mut expr = match_expr(
        local_expr(0, enum_ty),
        vec![
            arm(
                NirPattern::Enum {
                    enum_type: enum_ty,
                    case_name: "Case".to_string(),
                    case_index: 3,
                },
                bool_lit(true),
            ),
            arm(NirPattern::Wildcard, bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Binary { left, op, right } = &expr.kind else {
        panic!("expected Binary, got {:?}", expr.kind);
    };
    assert!(matches!(op, NirBinaryOp::Eq));
    let NirExprKind::Local { index, .. } = left.kind else {
        panic!("expected Local on left, got {:?}", left.kind);
    };
    assert_eq!(index, 0);
    let NirExprKind::EnumConstruct {
        case_index,
        case_name,
        ..
    } = &right.kind
    else {
        panic!("expected EnumConstruct on right, got {:?}", right.kind);
    };
    assert_eq!(*case_index, 3);
    assert_eq!(case_name, "Case");
}

#[test]
fn reduce_local_leaves_variant_match_alone() {
    // `match X { Some(_) => true, _ => false }` over a `NirPattern::Variant`
    // is left intact for now — synthesising the matching `VariantTest`
    // requires a variant→case-index registry the interpreter doesn't
    // carry. Tracked as a follow-up; the `Enum` arm above is what the
    // fpfmt motivator (`SpecialKind`) needs.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let mut expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(
                NirPattern::Variant {
                    enum_type: scrut_ty,
                    variant_name: "Some".to_string(),
                    bindings: vec![NirPattern::Wildcard],
                    payload_type: TypeTable::I32,
                },
                bool_lit(true),
            ),
            arm(NirPattern::Wildcard, bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_leaves_match_with_guard_alone() {
    // A guarded arm forces the fallthrough to depend on the guard's
    // runtime value; collapsing to a discriminator test would lose
    // that gate. Stay structurally intact.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let mut expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm_with_guard(
                NirPattern::Enum {
                    enum_type: scrut_ty,
                    case_name: "Case".to_string(),
                    case_index: 1,
                },
                local_expr(2, TypeTable::BOOL),
                bool_lit(true),
            ),
            arm(NirPattern::Wildcard, bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_leaves_match_with_three_arms_alone() {
    // The rule targets the specific two-arm `P => true, _ => false`
    // shape. Three arms (even with bool-literal bodies) fall outside
    // the rewrite — the second arm pattern is not the catch-all
    // wildcard.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let mut expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(
                NirPattern::Enum {
                    enum_type: scrut_ty,
                    case_name: "A".to_string(),
                    case_index: 0,
                },
                bool_lit(true),
            ),
            arm(
                NirPattern::Enum {
                    enum_type: scrut_ty,
                    case_name: "B".to_string(),
                    case_index: 1,
                },
                bool_lit(true),
            ),
            arm(NirPattern::Wildcard, bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_leaves_match_with_non_bool_body_alone() {
    // The rule requires both arm bodies to be bool literals. An int
    // body falls through to the all-arms-equal collapse (which doesn't
    // match since the bodies are distinct) and the match stays put.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let mut expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(
                NirPattern::Enum {
                    enum_type: scrut_ty,
                    case_name: "Case".to_string(),
                    case_index: 0,
                },
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_collapses_equal_arm_match_to_literal() {
    // Non-const speculatable scrutinee with all arms producing the same
    // Const collapses to that literal.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(NirPattern::Wildcard, int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral, got {:?}", expr.kind);
    };
    assert_eq!(value, 7);
}

#[test]
fn reduce_local_leaves_unequal_arm_match_alone() {
    // Different Const arms under a non-const scrutinee: the match is
    // not rewritten.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(1, TypeTable::I32, "1")),
            arm(NirPattern::Wildcard, int_lit(2, TypeTable::I32, "2")),
        ],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn reduce_local_recurses_into_match_arm_body() {
    // The driver path enters `reduce` (not `reduce_local`) which uses
    // `reduce_in_place` to recurse into children. The arm body
    // `1 + 2` should fold to `3` even when the surrounding match
    // doesn't itself collapse (here the arm body's reduction is
    // observable as the engine's lattice value).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let folded = interp.reduce(&match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm(
            lit_pat_i128(1),
            binary(
                NirBinaryOp::Add,
                int_lit(1, TypeTable::I32, "1"),
                int_lit(2, TypeTable::I32, "2"),
                TypeTable::I32,
            ),
        )],
        TypeTable::I32,
    ));
    // After reduce: the match collapsed to Block([Expr(3)]).
    let NirExprKind::Block(b) = &folded.kind else {
        panic!("expected Block, got {:?}", folded.kind);
    };
    let NirStmtKind::Expr(tail) = &b.stmts[0].kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = tail.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 3);
}

#[test]
fn match_const_char_range_inclusive() {
    // `match 'm' { 'a'..='z' => 1, _ => 0 }` → 1 (chars compare by
    // codepoint).
    let table = TypeTable::new();
    let expr = match_expr(
        char_lit('m'),
        vec![
            arm(
                range_pat(
                    i128::from(u32::from('a')),
                    i128::from(u32::from('z')),
                    true,
                    false,
                ),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_char_range_excludes_outside() {
    // `match '0' { 'a'..='z' => 1, _ => 0 }` → 0 ('0' is below 'a').
    let table = TypeTable::new();
    let expr = match_expr(
        char_lit('0'),
        vec![
            arm(
                range_pat(
                    i128::from(u32::from('a')),
                    i128::from(u32::from('z')),
                    true,
                    false,
                ),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_constant_value_pattern_against_const_int() {
    // `ConstantValue { expr: 42 }` matches scrut == 42 → Yes.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(42, TypeTable::I32, "42"),
        vec![
            arm(
                NirPattern::ConstantValue {
                    expr: Box::new(int_lit(42, TypeTable::I32, "42")),
                },
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_constant_value_pattern_misses() {
    // `ConstantValue { expr: 99 }` against scrut == 42 → No.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(42, TypeTable::I32, "42"),
        vec![
            arm(
                NirPattern::ConstantValue {
                    expr: Box::new(int_lit(99, TypeTable::I32, "99")),
                },
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_constant_value_pattern_unanalyzable_is_unknown() {
    // ConstantValue whose expr is a Local (Unevaluated) is Unknown
    // — the engine can't decide. With const scrut, the rewrite
    // bails out, keeping the Match intact.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(42, TypeTable::I32, "42"),
        vec![arm(
            NirPattern::ConstantValue {
                expr: Box::new(local_expr(0, TypeTable::I32)),
            },
            int_lit(1, TypeTable::I32, "1"),
        )],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn match_or_pattern_no_match_no_unknowns_is_definite_no() {
    // Or-pattern with all definite-No alternatives reports No, so
    // a wildcard later catches the scrut. With const scrut == 99
    // and `Or([1, 2])` arm, the engine drops the Or arm and picks
    // the wildcard — `reduce_local` rewrites the match to the
    // wildcard's body block.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        int_lit(99, TypeTable::I32, "99"),
        vec![
            arm(
                NirPattern::Or(vec![lit_pat_i128(1), lit_pat_i128(2)]),
                int_lit(10, TypeTable::I32, "10"),
            ),
            arm(NirPattern::Wildcard, int_lit(20, TypeTable::I32, "20")),
        ],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::Block(b) = &expr.kind else {
        panic!("expected Block");
    };
    let NirStmtKind::Expr(tail) = &b.stmts[0].kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = tail.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 20);
}

#[test]
fn match_signed_negative_against_unsigned_range_is_no() {
    // A signed negative scrutinee (e.g. -1 in i32) can never be in
    // an unsigned range like `0..=255`. Engine returns definite No
    // for that arm, falling through to the wildcard.
    let table = TypeTable::new();
    let neg_one_bits = i64::from(-1_i32) as u64;
    let expr = match_expr(
        int_lit(neg_one_bits, TypeTable::I32, "-1"),
        vec![
            arm(
                range_pat(0, 255, true, true),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(NirPattern::Wildcard, int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_int_picks_first_of_overlapping_arms() {
    // First-match wins: arms `1`, `1..=5` both match scrut 1; the
    // engine commits to the first.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(
                range_pat(1, 5, true, false),
                int_lit(20, TypeTable::I32, "20"),
            ),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 10,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_via_env_resolved_local_scrutinee() {
    // Scrutinee is a `Local` bound to `Const(2)` in env. The match
    // should still fold via the lattice path (as with `if`).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(
        0,
        Lattice::Const(Value::Int {
            value: 2,
            prim: PrimitiveType::I32,
        }),
    );
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(lit_pat_i128(2), int_lit(20, TypeTable::I32, "20")),
            arm(NirPattern::Wildcard, int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        interp.reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 20,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn match_const_scrut_with_only_or_pattern_arms_picks_first() {
    // No wildcard, but the or-pattern covers the scrut value exactly.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(2, TypeTable::I32, "2"),
        vec![
            arm(
                NirPattern::Or(vec![lit_pat_i128(1), lit_pat_i128(2)]),
                int_lit(10, TypeTable::I32, "10"),
            ),
            arm(lit_pat_i128(3), int_lit(20, TypeTable::I32, "20")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::Const(Value::Int {
            value: 10,
            prim: PrimitiveType::I32,
        }),
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// `match` exhaustiveness gate
//
// Without an unguarded catch-all the lowering inserts an `Unreachable`
// fallback for unmatched scrutinee values. Wado's resolver enforces
// match exhaustiveness for `bool` / `enum` / `variant` / range-covered
// `int`, but explicitly skips it for `struct` / `string` / `tuple` / …
// The engine must not collapse a non-exhaustive match to a literal
// (it would erase the trap), nor return a `Const(_)` lattice for one
// (a downstream `if` collapse could erase the trap on its behalf).
// ──────────────────────────────────────────────────────────────────────────────

fn binding_pat(name: &str, local_index: u32, type_id: TypeId) -> NirPattern {
    NirPattern::Binding {
        name: name.to_string(),
        local_index,
        type_id,
    }
}

#[test]
fn match_nonconst_scrut_non_exhaustive_all_arms_equal_does_not_rewrite() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(lit_pat_i128(2), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

#[test]
fn match_nonconst_scrut_non_exhaustive_lattice_is_nonconst() {
    let table = TypeTable::new();
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(lit_pat_i128(2), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_nonconst_scrut_exhaustive_wildcard_collapses() {
    // Sanity: with an unguarded wildcard the match IS exhaustive,
    // so the gate lets the all-arms-equal collapse fire.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(NirPattern::Wildcard, int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 7);
}

#[test]
fn match_const_scrut_only_unknown_arms_lattice_is_nonconst() {
    // Const scrutinee, but the only arm has an Unknown pattern (Tuple
    // here, since Phase A doesn't model tuple patterns). No definite
    // Yes is found, so the runtime may fall through to the trap. The
    // lattice must report `NonConst` — *not* `Const(99)` — even
    // though the only candidate body is `Const(99)`.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm(
            NirPattern::Tuple(vec![], false),
            int_lit(99, TypeTable::I32, "99"),
        )],
        TypeTable::I32,
    );
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice(&expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_nonconst_scrut_or_pattern_with_embedded_wildcard_is_exhaustive() {
    // `Or([1, _])` contains an unguarded wildcard alternative; the
    // engine should recognize it as a catch-all and let the
    // collapse fire.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![arm(
            NirPattern::Or(vec![lit_pat_i128(1), NirPattern::Wildcard]),
            int_lit(7, TypeTable::I32, "7"),
        )],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 7);
}

#[test]
fn match_nonconst_scrut_binding_pattern_counts_as_exhaustive() {
    // A `Binding` pattern always matches and captures the value —
    // a catch-all by another name.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(
                binding_pat("x", 1, TypeTable::I32),
                int_lit(7, TypeTable::I32, "7"),
            ),
        ],
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 7);
}

#[test]
fn match_nonconst_scrut_guarded_wildcard_does_not_count_as_exhaustive() {
    // A guarded catch-all is NOT exhaustive — if the guard fails,
    // control falls through to the implicit trap.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm_with_guard(
                NirPattern::Wildcard,
                local_expr(1, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
        ],
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Match { .. }));
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure call inlining (try_call_fold)
// ──────────────────────────────────────────────────────────────────────────────

/// Build a minimal `NirFunction` with the supplied signature and a
/// single-statement body. `body_stmt` is normally `NirStmtKind::Return`
/// or `NirStmtKind::Expr`. The function is module-public, non-async,
/// non-CM, non-method, non-generic — i.e. CTFE-eligible by default.
fn make_pure_fn(
    name: &str,
    params: Vec<(&str, TypeId)>,
    return_type: TypeId,
    body_stmt: NirStmtKind,
) -> NirFunction {
    let span = Span::default();
    let tir_params: Vec<NirParam> = params
        .iter()
        .enumerate()
        .map(|(i, (n, ty))| NirParam {
            name: (*n).to_string(),
            type_id: *ty,
            #[allow(clippy::cast_possible_truncation)]
            local_index: i as u32,
            is_mut: false,
            default_expr: None,
            span,
        })
        .collect();
    let locals: Vec<NirLocal> = params
        .iter()
        .map(|(n, ty)| NirLocal {
            name: (*n).to_string(),
            type_id: *ty,
            is_mut: false,
        })
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let local_count = params.len() as u32;
    NirFunction {
        name: name.to_string(),
        module_source: ModuleSource::default(),
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: tir_params,
        return_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: Vec::new(),
        body: Some(NirBlock::new(vec![NirStmt::new(body_stmt, span)], span)),
        span,
        local_count,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
        return_abi: ReturnAbi::Single,
    }
}

/// Build a `Call` expression targeting `func` with the given args.
/// Mirrors what the resolver emits for a free function call.
fn call_expr(func: &NirFunction, args: Vec<NirExpr>) -> NirExpr {
    let func_ref = FunctionRef {
        module_source: func.module_source.clone(),
        name: func.name.clone(),
        monomorph_info: None,
        method_info: None,
    };
    let call_args = args.into_iter().map(|e| CallArg::new(e, false)).collect();
    NirExpr::new(
        NirExprKind::Call {
            func: func_ref,
            type_args: Vec::new(),
            args: call_args,
        },
        func.return_type,
        Span::default(),
    )
}

/// Build a `CalleeMap` from the supplied functions, wrapping each in
/// `Rc<RefCell<...>>` to match the production map shape.
fn build_callee_map_test(funcs: &[NirFunction]) -> CalleeMap {
    let mut map = CalleeMap::default();
    for f in funcs {
        let key = (
            f.module_source.clone(),
            FunctionRef::from_resolved(f, f.module_source.clone()).full_name(),
        );
        map.insert(key, Rc::new(RefCell::new(f.clone())));
    }
    map
}

#[test]
fn pure_call_const_args_folds_via_return() {
    // fn double(x: i32) -> i32 { return x * 2; }
    // double(5) → 10
    let body = NirStmtKind::Return {
        value: Some(binary(
            NirBinaryOp::Mul,
            local_expr(0, TypeTable::I32),
            int_lit(2, TypeTable::I32, "2"),
            TypeTable::I32,
        )),
    };
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&double, vec![int_lit(5, TypeTable::I32, "5")]);
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral, got {:?}", expr.kind);
    };
    assert_eq!(value, 10);
}

#[test]
fn pure_call_const_args_folds_via_tail_expr() {
    // fn add(a: i32, b: i32) -> i32 { a + b }   (expression-bodied)
    let body = NirStmtKind::Expr(binary(
        NirBinaryOp::Add,
        local_expr(0, TypeTable::I32),
        local_expr(1, TypeTable::I32),
        TypeTable::I32,
    ));
    let add = make_pure_fn(
        "add",
        vec![("a", TypeTable::I32), ("b", TypeTable::I32)],
        TypeTable::I32,
        body,
    );
    let callees = build_callee_map_test(std::slice::from_ref(&add));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(
        &add,
        vec![
            int_lit(40, TypeTable::I32, "40"),
            int_lit(2, TypeTable::I32, "2"),
        ],
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 42);
}

#[test]
fn pure_call_chained_folds_two_levels() {
    // fn double(x) { return x * 2 }
    // We test bottom-up chaining: fold the inner call first, then
    // the outer wraps the now-literal arg and folds again.
    let body = NirStmtKind::Return {
        value: Some(binary(
            NirBinaryOp::Mul,
            local_expr(0, TypeTable::I32),
            int_lit(2, TypeTable::I32, "2"),
            TypeTable::I32,
        )),
    };
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut inner = call_expr(&double, vec![int_lit(3, TypeTable::I32, "3")]);
    assert!(interp.reduce_local(&mut inner));
    let mut outer = call_expr(&double, vec![inner]);
    assert!(interp.reduce_local(&mut outer));
    let NirExprKind::IntLiteral { value, .. } = outer.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 12);
}

#[test]
fn pure_call_nonconst_arg_left_intact() {
    // double(x) where x has no env binding — arg is Unevaluated, so
    // the call must not be folded.
    let body = NirStmtKind::Return {
        value: Some(binary(
            NirBinaryOp::Mul,
            local_expr(0, TypeTable::I32),
            int_lit(2, TypeTable::I32, "2"),
            TypeTable::I32,
        )),
    };
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&double, vec![local_expr(7, TypeTable::I32)]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn non_pure_call_with_effect_left_intact() {
    // A function carrying any effect is not CTFE-eligible — the
    // CalleeMap excludes it, so the call stays a Call.
    let body = NirStmtKind::Return {
        value: Some(int_lit(42, TypeTable::I32, "42")),
    };
    let mut greet = make_pure_fn("greet", vec![], TypeTable::I32, body);
    greet.effects.push(EffectRef::Concrete {
        name: "Stdout".to_string(),
        module_source: ModuleSource::default(),
    });
    assert!(!is_ctfe_eligible(&greet));
    let callees = build_callee_map_test(&[]); // greet not admitted

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&greet, vec![]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn multi_stmt_body_left_intact() {
    // fn f(x) { let y = x * 2; return y; }
    // The recognized body shape is single-stmt only, so the fold
    // declines.
    let body_block = NirBlock::new(
        vec![
            NirStmt::new(
                NirStmtKind::Let {
                    name: "y".to_string(),
                    local_index: 1,
                    is_mut: false,
                    is_reactive: false,
                    type_id: TypeTable::I32,
                    value: binary(
                        NirBinaryOp::Mul,
                        local_expr(0, TypeTable::I32),
                        int_lit(2, TypeTable::I32, "2"),
                        TypeTable::I32,
                    ),
                    skip_value_copy: false,
                },
                Span::default(),
            ),
            NirStmt::new(
                NirStmtKind::Return {
                    value: Some(local_expr(1, TypeTable::I32)),
                },
                Span::default(),
            ),
        ],
        Span::default(),
    );
    let mut f = make_pure_fn(
        "f",
        vec![("x", TypeTable::I32)],
        TypeTable::I32,
        NirStmtKind::Return { value: None }, // placeholder, replaced below
    );
    f.body = Some(body_block);
    f.local_count = 2;
    f.locals.push(NirLocal {
        name: "y".to_string(),
        type_id: TypeTable::I32,
        is_mut: false,
    });

    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&f, vec![int_lit(3, TypeTable::I32, "3")]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn recursive_call_bails_via_call_stack() {
    // fn f(x) { return f(x); } — direct self-recursion. The
    // `call_stack` guard refuses re-entry on the same key, so the
    // inner `f` evaluates to Unevaluated and the outer call therefore
    // stays unfolded as well.
    let body = NirStmtKind::Return {
        value: Some(NirExpr::new(
            NirExprKind::Unit,
            TypeTable::I32,
            Span::default(),
        )),
    };
    let mut f = make_pure_fn("f", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let self_call = call_expr(&f, vec![local_expr(0, TypeTable::I32)]);
    f.body = Some(NirBlock::new(
        vec![NirStmt::new(
            NirStmtKind::Return {
                value: Some(self_call),
            },
            Span::default(),
        )],
        Span::default(),
    ));

    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&f, vec![int_lit(1, TypeTable::I32, "1")]);
    // Must not fold; must terminate.
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn step_budget_zero_bails() {
    // With budget set to 0 up-front, even a trivially-foldable call
    // declines. Verifies the budget gate is reached before the body
    // is touched.
    let body = NirStmtKind::Return {
        value: Some(local_expr(0, TypeTable::I32)),
    };
    let id = make_pure_fn("id", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&id));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees).set_step_budget(0);

    let mut expr = call_expr(&id, vec![int_lit(7, TypeTable::I32, "7")]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn body_traps_at_ctfe_left_intact() {
    // fn bad() -> i32 { return 1 / 0; }
    // The body folds to NonConst (div-by-zero), which try_call_fold
    // downgrades to Unevaluated to keep the runtime trap intact.
    let body = NirStmtKind::Return {
        value: Some(binary(
            NirBinaryOp::Div,
            int_lit(1, TypeTable::I32, "1"),
            int_lit(0, TypeTable::I32, "0"),
            TypeTable::I32,
        )),
    };
    let bad = make_pure_fn("bad", vec![], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&bad));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&bad, vec![]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn missing_callee_left_intact() {
    // CalleeMap empty → look-up miss → no fold.
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let f = make_pure_fn("f", vec![], TypeTable::I32, body);
    let callees = build_callee_map_test(&[]); // empty

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut expr = call_expr(&f, vec![]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn no_callee_map_means_no_fold() {
    // Without `with_callees`, every Call is Unevaluated.
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let f = make_pure_fn("f", vec![], TypeTable::I32, body);

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);

    let mut expr = call_expr(&f, vec![]);
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Call { .. }));
}

#[test]
fn ctfe_eligibility_rejects_async() {
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.is_async = true;
    assert!(!is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_rejects_inline_never() {
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.inline_hint = InlineHint::Never;
    assert!(!is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_rejects_no_body() {
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.body = None;
    assert!(!is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_accepts_default_pure_fn() {
    let body = NirStmtKind::Return {
        value: Some(int_lit(1, TypeTable::I32, "1")),
    };
    let f = make_pure_fn("f", vec![], TypeTable::I32, body);
    assert!(is_ctfe_eligible(&f));
}

#[test]
fn pure_call_in_if_arm_folds_via_outer_walk() {
    // Verifies that try_call_fold composes with the outer
    // `if`-rewrite path. Build:
    //   if true { double(5) } else { 0 }
    // With the visitor's bottom-up walk, double(5) folds to 10
    // first; then the if collapses to the then-arm.
    let body = NirStmtKind::Return {
        value: Some(binary(
            NirBinaryOp::Mul,
            local_expr(0, TypeTable::I32),
            int_lit(2, TypeTable::I32, "2"),
            TypeTable::I32,
        )),
    };
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let mut inner = call_expr(&double, vec![int_lit(5, TypeTable::I32, "5")]);
    assert!(interp.reduce_local(&mut inner));
    assert!(matches!(inner.kind, NirExprKind::IntLiteral { .. }));

    let mut if_expr = NirExpr::new(
        NirExprKind::If {
            condition: Box::new(bool_lit(true)),
            then_branch: NirBlock::new(
                vec![NirStmt::new(NirStmtKind::Expr(inner), Span::default())],
                Span::default(),
            ),
            else_branch: Some(NirBlock::new(
                vec![NirStmt::new(
                    NirStmtKind::Expr(int_lit(0, TypeTable::I32, "0")),
                    Span::default(),
                )],
                Span::default(),
            )),
        },
        TypeTable::I32,
        Span::default(),
    );
    assert!(interp.reduce_local(&mut if_expr));
    let NirExprKind::Block(block) = &if_expr.kind else {
        panic!("expected Block, got {:?}", if_expr.kind);
    };
    let [stmt] = block.stmts.as_slice() else {
        panic!("expected single stmt");
    };
    let NirStmtKind::Expr(e) = &stmt.kind else {
        panic!("expected Expr stmt");
    };
    let NirExprKind::IntLiteral { value, .. } = e.kind else {
        panic!("expected IntLiteral");
    };
    assert_eq!(value, 10);
}

// ──────────────────────────────────────────────────────────────────────────────
// Stage 1 (extended): GlobalEnv — `GlobalVarGet` rewriting and lattice lookup
// ──────────────────────────────────────────────────────────────────────────────

fn global_get(module: ModuleSource, name: &str, type_id: TypeId) -> NirExpr {
    NirExpr::new(
        NirExprKind::GlobalVarGet {
            module_source: module,
            name: name.to_string(),
        },
        type_id,
        Span::default(),
    )
}

#[test]
fn global_const_int_folds_via_reduce_local() {
    // `global X: i32 = 42;` → reading `X` rewrites to `42`.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut globals = GlobalEnv::default();
    globals.insert(
        (module.clone(), "X".to_string()),
        Lattice::Const(Value::Int {
            value: 42,
            prim: PrimitiveType::I32,
        }),
    );

    let mut interp = Interpreter::new(&table);
    interp.with_globals(&globals);

    let mut expr = global_get(module, "X", TypeTable::I32);
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral, got {:?}", expr.kind);
    };
    assert_eq!(value, 42);
}

#[test]
fn global_const_threads_into_binary_fold() {
    // `global X: i32 = 10;` → `X + 5` folds to `15`.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut globals = GlobalEnv::default();
    globals.insert(
        (module.clone(), "X".to_string()),
        Lattice::Const(Value::Int {
            value: 10,
            prim: PrimitiveType::I32,
        }),
    );

    let mut interp = Interpreter::new(&table);
    interp.with_globals(&globals);

    let mut expr = binary(
        NirBinaryOp::Add,
        global_get(module, "X", TypeTable::I32),
        int_lit(5, TypeTable::I32, "5"),
        TypeTable::I32,
    );
    assert!(interp.reduce_local(&mut expr));
    let NirExprKind::IntLiteral { value, .. } = expr.kind else {
        panic!("expected IntLiteral, got {:?}", expr.kind);
    };
    assert_eq!(value, 15);
}

#[test]
fn global_mut_recorded_as_nonconst_blocks_fold() {
    // `global mut X: i32 = 0;` → `X + 5` stays as Binary, not folded.
    // Records the local as `NonConst` so the parent fold reports
    // `NonConst` rather than `Unevaluated`.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut globals = GlobalEnv::default();
    globals.insert((module.clone(), "X".to_string()), Lattice::NonConst);

    let mut interp = Interpreter::new(&table);
    interp.with_globals(&globals);

    let lat = interp.reduce_to_lattice(&global_get(module.clone(), "X", TypeTable::I32));
    assert_eq!(lat, Lattice::NonConst);

    let mut expr = binary(
        NirBinaryOp::Add,
        global_get(module, "X", TypeTable::I32),
        int_lit(5, TypeTable::I32, "5"),
        TypeTable::I32,
    );
    assert!(!interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::Binary { .. }));
}

#[test]
fn global_absent_stays_unevaluated() {
    // No `with_globals` installed → `GlobalVarGet` reports `Unevaluated`
    // (engine has no information). Same convention as un-bound locals.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut interp = Interpreter::new(&table);
    let lat = interp.reduce_to_lattice(&global_get(module.clone(), "MISSING", TypeTable::I32));
    assert_eq!(lat, Lattice::Unevaluated);

    // With an empty `GlobalEnv` installed, an unknown key still reports
    // `Unevaluated` — no NonConst materializes spuriously.
    let globals = GlobalEnv::default();
    interp.with_globals(&globals);
    let lat = interp.reduce_to_lattice(&global_get(module, "MISSING", TypeTable::I32));
    assert_eq!(lat, Lattice::Unevaluated);
}

#[test]
fn global_const_bool_folds_via_reduce_local() {
    // `global ENABLED: bool = true;` — covers the non-int path.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut globals = GlobalEnv::default();
    globals.insert(
        (module.clone(), "ENABLED".to_string()),
        Lattice::Const(Value::Bool(true)),
    );

    let mut interp = Interpreter::new(&table);
    interp.with_globals(&globals);

    let mut expr = global_get(module, "ENABLED", TypeTable::BOOL);
    assert!(interp.reduce_local(&mut expr));
    assert!(matches!(expr.kind, NirExprKind::BoolLiteral(true)));
}
