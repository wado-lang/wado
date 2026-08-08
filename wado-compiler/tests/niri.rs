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
use wado_compiler::compiler_item::SeqField;
use wado_compiler::const_eval::{MAX_SEQ_ELEMENTS, Value};
use wado_compiler::hashmap::IndexSet;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::nir::{
    FunctionKind, InlineHint, NirBinaryOp, NirFunction, NirLiteralPattern, NirLocal, NirParam,
    NirUnaryOp, ReturnAbi,
};
use wado_compiler::nir_arena::{
    ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode, Body, ExprId, ExprKind,
    ExprNode, Operand, PatId, PatKind, PatNode, StmtId, StmtKind, StmtNode,
};
use wado_compiler::nir_value_graph::ValueKind;
use wado_compiler::niri::{
    BodySink, Callee, CalleeMap, CtfeBuiltin, CtfeBuiltinMap, DEFAULT_STEP_BUDGET, GlobalEnv,
    GlobalFieldEnv, Interpreter, Lattice, is_ctfe_eligible,
};
use wado_compiler::tir::{EffectRef, PrimitiveType, TypeId, TypeTable};

/// The conveniences these tests want on top of the engine's own API: a
/// reduction that drives both halves, and the two rewrites bound to the scratch
/// [`BodySink`].
///
/// Here rather than in the engine, whose own callers drive the halves
/// separately and supply their own sink — the const-fold visitor commits
/// through the optimizer's engine so the real body's maps stay coherent.
trait ScratchReduce {
    fn reduce_to_lattice_full(&mut self, body: &mut Body, e: ExprId) -> Lattice;
    fn reduce_local_in_body(&mut self, body: &mut Body, e: ExprId) -> bool;
    fn reduce_local_block_in_body(&mut self, body: &mut Body, block: BlockId) -> bool;
}

impl ScratchReduce for Interpreter<'_> {
    fn reduce_to_lattice_full(&mut self, body: &mut Body, e: ExprId) -> Lattice {
        self.reduce_in_place(body, e);
        self.reduce_to_lattice(body, e)
    }

    fn reduce_local_in_body(&mut self, body: &mut Body, e: ExprId) -> bool {
        self.reduce_local(&mut BodySink { body }, e)
    }

    fn reduce_local_block_in_body(&mut self, body: &mut Body, block: BlockId) -> bool {
        self.reduce_local_block(&mut BodySink { body }, block)
    }
}

/// A deferred operand builder: appends any needed subtree to the arena `Body`
/// and returns the operand it produces — a pooled `Operand::Value` for a pure
/// scalar literal, or `Operand::Expr` for a composite. `Rc` so a builder can be
/// cloned and re-used (an operand shared by two parents).
type Build = Rc<dyn Fn(&mut Body) -> Operand>;

fn pe(body: &mut Body, kind: ExprKind, type_id: TypeId) -> ExprId {
    body.exprs.push(ExprNode {
        kind,
        type_id,
        span: Span::default(),
    })
}

fn char_lit(c: char) -> Build {
    Rc::new(move |b| Operand::Value(b.values.alloc_unshared(ValueKind::Char(c), TypeTable::CHAR)))
}

fn cast_expr(inner: Build, target_ty: TypeId) -> Build {
    Rc::new(move |b| {
        let e = inner(b);
        Operand::Expr(pe(
            b,
            ExprKind::Cast {
                expr: e,
                target_type: target_ty,
            },
            target_ty,
        ))
    })
}

fn int_lit(value: u64, type_id: TypeId, _repr: &str) -> Build {
    Rc::new(move |b| {
        Operand::Value(
            b.values
                .alloc_unshared(ValueKind::Int(value, type_id), type_id),
        )
    })
}

fn float_lit(value: f64, type_id: TypeId, _repr: &str) -> Build {
    Rc::new(move |b| {
        Operand::Value(
            b.values
                .alloc_unshared(ValueKind::Float(value.to_bits(), type_id), type_id),
        )
    })
}

fn bool_lit(value: bool) -> Build {
    Rc::new(move |b| {
        Operand::Value(
            b.values
                .alloc_unshared(ValueKind::Bool(value), TypeTable::BOOL),
        )
    })
}

fn binary(op: NirBinaryOp, left: Build, right: Build, result_ty: TypeId) -> Build {
    Rc::new(move |b| {
        let l = left(b);
        let r = right(b);
        Operand::Expr(pe(
            b,
            ExprKind::Binary {
                left: l,
                op,
                right: r,
            },
            result_ty,
        ))
    })
}

fn unary(op: NirUnaryOp, expr: Build, result_ty: TypeId) -> Build {
    Rc::new(move |b| {
        let e = expr(b);
        Operand::Expr(pe(b, ExprKind::Unary { op, expr: e }, result_ty))
    })
}

fn local_expr(index: u32, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        Operand::Expr(pe(
            b,
            ExprKind::Local {
                index,
                name: format!("l{index}"),
            },
            type_id,
        ))
    })
}

/// Build `build` into a fresh `Body`, returning both so a caller can run an
/// interpreter method and then inspect the (possibly rewritten) node.
fn into_body(build: &Build) -> (Body, Operand) {
    let mut body = Body::empty();
    let op = build(&mut body);
    (body, op)
}

/// The `ExprId` of a composite-operand build (the common case for the reducer
/// tests, which inspect the rewritten node). Panics for a bare-constant build.
fn into_body_expr(build: &Build) -> (Body, ExprId) {
    let (body, op) = into_body(build);
    let e = op
        .as_expr()
        .expect("this harness reduces a composite expression, not a bare constant");
    (body, e)
}

/// The `u64` behind a constant `Operand::Value` whose pooled kind is `Int`.
/// Under operand promotion a folded / literal integer lives in the value pool,
/// not as an `ExprKind` node, so the reducer leaves it in the operand slot.
fn op_int(body: &Body, op: Operand) -> u64 {
    let Operand::Value(v) = op else {
        panic!("expected a constant value operand, got {op:?}");
    };
    match body.values.kind(v) {
        ValueKind::Int(n, _) => *n,
        other => panic!("expected Int value, got {other:?}"),
    }
}

/// Flow-fold `build`'s root expression to a constant via the supplied
/// interpreter (which may carry callees / globals / env). This is the value
/// the const-fold visitor's engine sink promotes into the operand slot in
/// production; under the scratch `BodySink` the node is left in place and the
/// fold is read back here directly.
fn flow_fold(interp: &mut Interpreter, build: &Build) -> Option<Value> {
    let (body, op) = into_body(build);
    let e = op
        .as_expr()
        .expect("expected a composite expression to flow-fold");
    interp.flow_fold_value(&body, e)
}

/// Full bottom-up reduction of a freshly-built expression to a lattice, using
/// a default interpreter (no env / callees). The arena analogue of the old
/// `reduce_lat(&mut Interpreter::new(&table), &expr)`.
fn lattice_of(build: &Build) -> Lattice {
    let table = TypeTable::new();
    reduce_lat(&mut Interpreter::new(&table), build)
}

/// Like [`lattice_of`] but against a caller-supplied (stateful) interpreter.
/// A composite build reduces in place then projects; a bare-constant build's
/// lattice is read straight from its pooled value operand.
fn reduce_lat(interp: &mut Interpreter, build: &Build) -> Lattice {
    let (mut body, op) = into_body(build);
    match op {
        Operand::Expr(e) => interp.reduce_to_lattice_full(&mut body, e),
        Operand::Value(_) => interp.operand_to_lattice(&body, op),
    }
}

/// Convenience wrapper used by the legacy "is this a Const?" tests: reduce and
/// project to `Option<Value>` via [`Lattice::as_const`]. Unevaluated and
/// `NonConst` both collapse to `None` here — when a test cares about the
/// distinction it pattern matches on [`Lattice`] directly.
fn eval(expr: &Build) -> Option<Value> {
    lattice_of(expr).as_const()
}

fn expect_int(expr: &Build, expected_value: u64, expected_prim: PrimitiveType) {
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

fn expect_float(expr: &Build, expected_value: f64, expected_prim: PrimitiveType) {
    let v = eval(expr).expect("expected reduction");
    match v {
        Value::Float { value, prim } => {
            assert_eq!(prim, expected_prim, "wrong float width");
            assert_eq!(value, expected_value, "wrong float value");
        }
        other => panic!("expected float, got {other:?}"),
    }
}

fn expect_bool(expr: &Build, expected: bool) {
    assert_eq!(eval(expr), Some(Value::Bool(expected)));
}

fn unit_lit() -> Build {
    Rc::new(|b| Operand::Value(b.values.alloc_unshared(ValueKind::Unit, TypeTable::UNIT)))
}

/// Build, run the in-place reducer (`reduce_in_place`, the arena analogue of
/// the old tree `reduce`) with a default interpreter, and return the arena so
/// the caller can inspect the rewritten root node via `body.exprs[e].kind`.
fn reduce_to_expr(build: &Build) -> (Body, ExprId) {
    let table = TypeTable::new();
    let (mut body, e) = into_body_expr(build);
    Interpreter::new(&table).reduce_in_place(&mut body, e);
    (body, e)
}

/// Like [`reduce_to_expr`] but against a caller-supplied (stateful)
/// interpreter, so env / callee bindings are visible to the reduction.
fn reduce_with(interp: &mut Interpreter, build: &Build) -> (Body, ExprId) {
    let (mut body, e) = into_body_expr(build);
    interp.reduce_in_place(&mut body, e);
    (body, e)
}

/// Build, apply the single-node `reduce_local_in_body` rewrite, and return the
/// `changed` flag plus the arena so the caller can inspect `body.exprs[e]`.
fn reduce_local_into(interp: &mut Interpreter, build: &Build) -> (bool, Body, ExprId) {
    let (mut body, e) = into_body_expr(build);
    let changed = interp.reduce_local_in_body(&mut body, e);
    (changed, body, e)
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
        unit_lit(),
        TypeTable::I32,
    );
    assert_eq!(eval(&e), None);
}

// ──────────────────────────────────────────────────────────────────────────────
// `reduce` returns a NirExpr — repr preservation and shape contracts
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn reduce_preserves_literal_value() {
    // A bare `0xFF` literal is a pooled value operand under promotion — it has
    // no `ExprKind` node form, so it survives reduction unchanged in its
    // operand slot, carrying its value and width.
    let lit = int_lit(0xFF, TypeTable::U8, "0xFF");
    let (body, op) = into_body(&lit);
    assert_eq!(op_int(&body, op), 0xFF);
}

#[test]
fn reduce_collapses_binary_to_literal() {
    // `20 + 22` folds to the constant 42. Under operand promotion the folded
    // scalar is a pooled value (the `BodySink` reducer leaves the binary node
    // in place and the fold is observable through the lattice), not a literal
    // `ExprKind`.
    let e = binary(
        NirBinaryOp::Add,
        int_lit(20, TypeTable::I32, "20"),
        int_lit(22, TypeTable::I32, "22"),
        TypeTable::I32,
    );
    expect_int(&e, 42, PrimitiveType::I32);
}

#[test]
fn reduce_short_circuits_or_false() {
    // `false || X` reduces to `X` even when `X` is non-constant.
    let lhs = bool_lit(false);
    let rhs = local_expr(0, TypeTable::BOOL);
    let e = binary(NirBinaryOp::Or, lhs, rhs, TypeTable::BOOL);
    let (body, e) = reduce_to_expr(&e);
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Local { index: 0, .. }),
        "false || X should reduce to X, got {:?}",
        body.exprs[e].kind
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Lattice API — three states are observable, projection collapses two
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn lattice_const_for_literal() {
    let lit = int_lit(42, TypeTable::I32, "42");
    let lat = lattice_of(&lit);
    assert!(matches!(
        lat,
        Lattice::Const(Value::Int {
            value: 42,
            prim: PrimitiveType::I32,
        }),
    ));
}

#[test]
fn lattice_const_for_unit() {
    // `()` denotes a value, with one inhabitant. It was outside the engine's
    // model until `Value::Unit` existed; "I have no value for this" is what
    // `lattice_unevaluated_for_unbound_local` pins instead.
    let e = unit_lit();
    assert_eq!(lattice_of(&e), Lattice::Const(Value::Unit));
}

#[test]
fn lattice_unevaluated_for_unbound_local() {
    // No bind_local call → reading the local is "I don't know yet",
    // not "I know it isn't const".
    let local = local_expr(0, TypeTable::I32);
    assert_eq!(lattice_of(&local), Lattice::Unevaluated);
}

#[test]
fn lattice_nonconst_for_div_by_zero() {
    // Both operands are Const, but the op evidently fails — that's
    // NonConst, distinct from Unevaluated.
    let e = binary(
        NirBinaryOp::Div,
        int_lit(1, TypeTable::I32, "1"),
        int_lit(0, TypeTable::I32, "0"),
        TypeTable::I32,
    );
    assert_eq!(lattice_of(&e), Lattice::NonConst);
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
        reduce_lat(&mut interp, &e),
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
    assert_eq!(reduce_lat(&mut interp, &e), Lattice::NonConst);
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
    assert_eq!(reduce_lat(&mut interp, &e), Lattice::NonConst);
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
    assert_eq!(reduce_lat(&mut interp, &e), Lattice::Unevaluated);
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
    let (body, e) = reduce_with(&mut interp, &local);
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Local { index: 0, .. }),
        "Local must stay structurally a Local; env lookup happens at parents only, got {:?}",
        body.exprs[e].kind,
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
        reduce_lat(&mut interp, &e),
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
    let cast_expr = cast_expr(local_expr(0, src_ty), target_ty);
    assert_eq!(
        reduce_lat(&mut interp, &cast_expr),
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
    let ce = cast_expr(local_expr(0, TypeTable::U32), TypeTable::I32);
    // u as i32 must equal -1 (sign-extended bit pattern in u64 form).
    let neg_one_bits = i64::from(-1_i32) as u64;
    assert_eq!(
        reduce_lat(&mut interp, &ce),
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
    let lhs = cast_expr(local_expr(0, TypeTable::U32), TypeTable::I32);
    let cmp = binary(
        NirBinaryOp::Eq,
        lhs,
        int_lit(neg_one_bits, TypeTable::I32, "-1"),
        TypeTable::BOOL,
    );
    assert_eq!(
        reduce_lat(&mut interp, &cmp),
        Lattice::Const(Value::Bool(true)),
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// `Lattice::join` and `if`-expression reduction
// ──────────────────────────────────────────────────────────────────────────────

/// A deferred block builder, the block-level counterpart of [`Build`].
type BlockBuild = Rc<dyn Fn(&mut Body) -> BlockId>;

fn block_with_tail_expr(e: Build) -> BlockBuild {
    Rc::new(move |b| {
        let id = e(b);
        let s = b.stmts.push(StmtNode {
            kind: StmtKind::Expr(id),
            span: Span::default(),
        });
        b.blocks.push(BlockNode {
            stmts: vec![s],
            span: Span::default(),
        })
    })
}

fn if_expr(
    condition: Build,
    then_branch: BlockBuild,
    else_branch: Option<BlockBuild>,
    type_id: TypeId,
) -> Build {
    Rc::new(move |b| {
        let condition = condition(b);
        let then_branch = then_branch(b);
        let else_branch = else_branch.as_ref().map(|eb| eb(b));
        Operand::Expr(pe(
            b,
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            },
            type_id,
        ))
    })
}

#[test]
fn lattice_join_idempotent_on_equal_consts() {
    let v = Lattice::Const(Value::Int {
        value: 7,
        prim: PrimitiveType::I32,
    });
    assert_eq!(v.clone().join(v.clone()), v);
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
    assert_eq!(Lattice::Unevaluated.join(c.clone()), c);
    assert_eq!(c.clone().join(Lattice::Unevaluated), c);
    assert_eq!(
        Lattice::Unevaluated.join(Lattice::Unevaluated),
        Lattice::Unevaluated
    );
}

#[test]
fn lattice_join_nonconst_is_absorbing() {
    let c = Lattice::Const(Value::Bool(true));
    assert_eq!(Lattice::NonConst.join(c.clone()), Lattice::NonConst);
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
    assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));
    assert_eq!(
        a.clone().join(b.clone()).join(c.clone()),
        a.clone().join(b.clone().join(c.clone())),
    );
    assert_eq!(b.clone().join(c.clone()).join(a.clone()), c.join(a).join(b),);
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut interp, &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::NonConst,
    );
}

#[test]
fn reduce_local_rewrites_const_true_if_to_block() {
    // The visitor-driven path: `reduce_local_in_body` rewrites the `If` in
    // place to a `Block` of the chosen branch. This is the rewrite that
    // subsumes the `if true` case from the legacy `const_branch_prune`.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = if_expr(
        bool_lit(true),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        Some(block_with_tail_expr(int_lit(20, TypeTable::I32, "20"))),
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Block(blk) = body.exprs[e].kind else {
        panic!("expected Block, got {:?}", body.exprs[e].kind);
    };
    assert_eq!(body.blocks[blk].stmts.len(), 1);
    let s0 = body.blocks[blk].stmts[0];
    let StmtKind::Expr(tail) = body.stmts[s0].kind else {
        panic!("expected Expr stmt");
    };
    assert_eq!(op_int(&body, tail), 10);
}

#[test]
fn reduce_local_rewrites_const_false_if_no_else_to_unit() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = if_expr(
        bool_lit(false),
        block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
        None,
        TypeTable::UNIT,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    // `if false {}` with no else evaluates to unit; the unit value has no node
    // form, so the skeleton result is an empty block.
    let ExprKind::Block(blk) = body.exprs[e].kind else {
        panic!("expected an empty Block, got {:?}", body.exprs[e].kind);
    };
    assert!(body.blocks[blk].stmts.is_empty());
}

#[test]
fn reduce_local_collapses_equal_arm_if_to_literal() {
    // `if cond { 7 } else { 7 }` is the constant 7 regardless of `cond`. The
    // equal-arm collapse promotes through the engine sink in production; the
    // value is observable here as the joined lattice of the two arms.
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(int_lit(7, TypeTable::I32, "7")),
        Some(block_with_tail_expr(int_lit(7, TypeTable::I32, "7"))),
        TypeTable::I32,
    );
    expect_int(&expr, 7, PrimitiveType::I32);
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
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(bool_lit(false))),
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Local { index, .. } = body.exprs[e].kind else {
        panic!(
            "expected Local (the original condition), got {:?}",
            body.exprs[e].kind
        );
    };
    assert_eq!(index, 0);
    assert_eq!(body.exprs[e].type_id, TypeTable::BOOL);
}

#[test]
fn reduce_local_rewrites_if_false_true_to_not_cond() {
    // `if cond { false } else { true }` → `!cond`. The Unary::Not wrap
    // preserves the same observable behaviour as the original `if` —
    // truth and falsity are swapped, evaluation order is identical
    // (cond is evaluated, then negated).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(false)),
        Some(block_with_tail_expr(bool_lit(true))),
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Unary { op, expr: inner } = body.exprs[e].kind else {
        panic!("expected Unary::Not, got {:?}", body.exprs[e].kind);
    };
    assert!(matches!(op, NirUnaryOp::Not));
    let ExprKind::Local { index, .. } = body.exprs[inner.as_expr().unwrap()].kind else {
        panic!(
            "expected Local inside Unary::Not, got {:?}",
            body.exprs[inner.as_expr().unwrap()].kind
        );
    };
    assert_eq!(index, 0);
    assert_eq!(body.exprs[e].type_id, TypeTable::BOOL);
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
            arm(wildcard_pat(), bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let expr = if_expr(
        impure_cond,
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(bool_lit(false))),
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    // After rewrite, the if is replaced by the (still-non-speculatable)
    // cond expression itself — the Match.
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Match { .. }),
        "expected the original Match condition to survive as the result, got {:?}",
        body.exprs[e].kind
    );
}

#[test]
fn reduce_local_leaves_if_mixed_bool_int_arms_alone() {
    // Defensive: when arms have different types (bool then-arm, int
    // else-arm) the bool-arms rule must not fire. The (Bool, Bool)
    // tuple pattern in the rule guards against this.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(bool_lit(true)),
        Some(block_with_tail_expr(int_lit(0, TypeTable::I32, "0"))),
        // Type intentionally mismatched between if-expr and arms — a
        // elaborator-level invariant, but we want the rule to stay silent
        // regardless.
        TypeTable::BOOL,
    );
    let before = {
        let (b, e) = into_body_expr(&expr);
        format!("{:?}", b.exprs[e].kind)
    };
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert_eq!(format!("{:?}", body.exprs[e].kind), before);
}

#[test]
fn reduce_local_block_splices_const_true_if_stmt() {
    // Stmt-form `if true { stmts… }` → splice stmts into the parent.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut body = Body::empty();
    let condition = bool_lit(true)(&mut body);
    let then_block = block_with_tail_expr(int_lit(99, TypeTable::I32, "99"))(&mut body);
    let else_block = block_with_tail_expr(int_lit(0, TypeTable::I32, "0"))(&mut body);
    let if_stmt = ps(
        &mut body,
        StmtKind::If {
            condition,
            then_block,
            else_block: Some(else_block),
        },
    );
    let block = body.blocks.push(BlockNode {
        stmts: vec![if_stmt],
        span: Span::default(),
    });
    assert!(interp.reduce_local_block_in_body(&mut body, block));
    assert_eq!(body.blocks[block].stmts.len(), 1);
    let s0 = body.blocks[block].stmts[0];
    let StmtKind::Expr(e) = body.stmts[s0].kind else {
        panic!("expected Expr stmt");
    };
    assert_eq!(op_int(&body, e), 99);
}

#[test]
fn reduce_local_block_drops_const_false_if_stmt_without_else() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut body = Body::empty();
    let condition = bool_lit(false)(&mut body);
    let then_block = block_with_tail_expr(int_lit(99, TypeTable::I32, "99"))(&mut body);
    let if_stmt = ps(
        &mut body,
        StmtKind::If {
            condition,
            then_block,
            else_block: None,
        },
    );
    let block = body.blocks.push(BlockNode {
        stmts: vec![if_stmt],
        span: Span::default(),
    });
    assert!(interp.reduce_local_block_in_body(&mut body, block));
    assert!(body.blocks[block].stmts.is_empty());
}

#[test]
fn reduce_local_block_leaves_nonconst_if_alone() {
    // Stmt-form `if cond { … }` with a non-literal condition is left
    // structurally intact — `reduce_local_block_in_body` must not touch it.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let mut body = Body::empty();
    let condition = local_expr(0, TypeTable::BOOL)(&mut body);
    let then_block = block_with_tail_expr(int_lit(99, TypeTable::I32, "99"))(&mut body);
    let if_stmt = ps(
        &mut body,
        StmtKind::If {
            condition,
            then_block,
            else_block: None,
        },
    );
    let block = body.blocks.push(BlockNode {
        stmts: vec![if_stmt],
        span: Span::default(),
    });
    assert!(!interp.reduce_local_block_in_body(&mut body, block));
    assert_eq!(body.blocks[block].stmts.len(), 1);
    let s0 = body.blocks[block].stmts[0];
    assert!(matches!(body.stmts[s0].kind, StmtKind::If { .. }));
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
fn cast_f64_nan_to_i32_traps_so_stays_nonconst() {
    let e = cast_expr(float_lit(f64::NAN, TypeTable::F64, "nan"), TypeTable::I32);
    assert_eq!(lattice_of(&e), Lattice::NonConst);
}

#[test]
fn cast_f64_huge_to_i32_traps_so_stays_nonconst() {
    let e = cast_expr(float_lit(1e30, TypeTable::F64, "1e30"), TypeTable::I32);
    assert_eq!(lattice_of(&e), Lattice::NonConst);
}

#[test]
fn cast_f64_neg_huge_to_i32_traps_so_stays_nonconst() {
    let e = cast_expr(float_lit(-1e30, TypeTable::F64, "-1e30"), TypeTable::I32);
    assert_eq!(lattice_of(&e), Lattice::NonConst);
}

#[test]
fn cast_f64_to_i8_wraps_through_the_i32_intermediate() {
    expect_int(
        &cast_expr(float_lit(300.7, TypeTable::F64, "300.7"), TypeTable::I8),
        44,
        PrimitiveType::I8,
    );
}

#[test]
fn cast_f64_to_u8_wraps_through_the_u32_intermediate() {
    expect_int(
        &cast_expr(float_lit(300.7, TypeTable::F64, "300.7"), TypeTable::U8),
        44,
        PrimitiveType::U8,
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
    // char does not implement Add — the elaborator rejects it, but if a
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
        reduce_lat(&mut interp, &e),
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
        reduce_lat(&mut interp, &e),
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
        reduce_lat(&mut interp, &e),
        Lattice::Const(Value::Float {
            value: 42.0,
            prim: PrimitiveType::F64
        })
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// `match` expression reduction (payload-free patterns)
// ──────────────────────────────────────────────────────────────────────────────

/// A deferred pattern builder.
type PatBuild = Rc<dyn Fn(&mut Body) -> PatId>;
/// A deferred match-arm builder.
type ArmBuild = Rc<dyn Fn(&mut Body) -> ArmData>;

fn pp(body: &mut Body, kind: PatKind) -> PatId {
    body.pats.push(PatNode {
        kind,
        span: Span::default(),
    })
}

fn match_expr(scrutinee: Build, arms: Vec<ArmBuild>, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let expr = scrutinee(b);
        let arms = arms.iter().map(|a| a(b)).collect();
        Operand::Expr(pe(b, ExprKind::Match { expr, arms }, type_id))
    })
}

fn arm(pattern: PatBuild, body: Build) -> ArmBuild {
    Rc::new(move |b| {
        let pattern = pattern(b);
        let body = body(b);
        ArmData {
            pattern,
            guard: None,
            body,
            span: Span::default(),
        }
    })
}

fn arm_with_guard(pattern: PatBuild, guard: Build, body: Build) -> ArmBuild {
    Rc::new(move |b| {
        let pattern = pattern(b);
        let guard = Some(guard(b));
        let body = body(b);
        ArmData {
            pattern,
            guard,
            body,
            span: Span::default(),
        }
    })
}

fn wildcard_pat() -> PatBuild {
    Rc::new(|b| pp(b, PatKind::Wildcard))
}

fn lit_pat_i128(value: i128) -> PatBuild {
    Rc::new(move |b| pp(b, PatKind::Literal(NirLiteralPattern::I128(value))))
}

fn lit_pat_u128(value: u128) -> PatBuild {
    Rc::new(move |b| pp(b, PatKind::Literal(NirLiteralPattern::U128(value))))
}

fn lit_pat_bool(value: bool) -> PatBuild {
    Rc::new(move |b| pp(b, PatKind::Literal(NirLiteralPattern::Bool(value))))
}

fn lit_pat_char(value: char) -> PatBuild {
    Rc::new(move |b| pp(b, PatKind::Literal(NirLiteralPattern::Char(value))))
}

fn range_pat(start: i128, end: i128, inclusive: bool, is_unsigned: bool) -> PatBuild {
    Rc::new(move |b| {
        pp(
            b,
            PatKind::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            },
        )
    })
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
            arm(wildcard_pat(), int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
                or_pat(vec![
                    lit_pat_char('a'),
                    lit_pat_char('b'),
                    lit_pat_char('c'),
                ]),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
                or_pat(vec![lit_pat_char('a'), lit_pat_char('b')]),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
                wildcard_pat(),
                local_expr(0, TypeTable::I32), // unbound → Unevaluated
            ),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(2, TypeTable::I32, "2")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), local_expr(1, TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm_with_guard(
            lit_pat_i128(1),
            local_expr(0, TypeTable::BOOL),
            int_lit(7, TypeTable::I32, "7"),
        )],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn match_guarded_arm_blocks_rewrite_to_later_definite_arm() {
    // The guarded arm could fire if the guard succeeds, so the
    // engine cannot skip it and pick the later wildcard arm. The
    // match must be left structurally intact.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![
            arm_with_guard(
                lit_pat_i128(1),
                local_expr(0, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
            arm(wildcard_pat(), int_lit(8, TypeTable::I32, "8")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn match_guarded_arm_two_distinct_arm_bodies_is_nonconst() {
    // Lattice-level check: when a guarded arm and a later definite arm produce
    // different Const bodies, the merged lattice goes to NonConst (the value
    // depends on whether the guard fires). The scrutinee is a non-constant
    // local so the lattice join over the arms runs — a promoted-constant
    // scrutinee is instead collapsed structurally by the flow-fold visitor.
    let table = TypeTable::new();
    let expr = match_expr(
        local_expr(1, TypeTable::I32),
        vec![
            arm_with_guard(
                lit_pat_i128(1),
                local_expr(0, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
            arm(wildcard_pat(), int_lit(8, TypeTable::I32, "8")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm(
            tuple_pat(vec![], false),
            int_lit(99, TypeTable::I32, "99"),
        )],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn reduce_local_rewrites_const_match_to_arm_body_block() {
    // The visitor-driven path: `reduce_local_in_body` rewrites a constant-scrut
    // `Match` in place to a `Block` containing the chosen arm's body
    // expression as a single tail statement.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        int_lit(2, TypeTable::I32, "2"),
        vec![
            arm(lit_pat_i128(1), int_lit(10, TypeTable::I32, "10")),
            arm(lit_pat_i128(2), int_lit(20, TypeTable::I32, "20")),
            arm(wildcard_pat(), int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Block(blk) = body.exprs[e].kind else {
        panic!("expected Block, got {:?}", body.exprs[e].kind);
    };
    assert_eq!(body.blocks[blk].stmts.len(), 1);
    let s0 = body.blocks[blk].stmts[0];
    let StmtKind::Expr(tail) = body.stmts[s0].kind else {
        panic!("expected Expr stmt");
    };
    assert_eq!(op_int(&body, tail), 20);
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
    let expr = match_expr(
        local_expr(0, enum_ty),
        vec![
            arm(enum_pat(enum_ty, "Case", 3), bool_lit(true)),
            arm(wildcard_pat(), bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Binary { left, op, right } = body.exprs[e].kind else {
        panic!("expected Binary, got {:?}", body.exprs[e].kind);
    };
    assert!(matches!(op, NirBinaryOp::Eq));
    let ExprKind::Local { index, .. } = body.exprs[left.as_expr().unwrap()].kind else {
        panic!(
            "expected Local on left, got {:?}",
            body.exprs[left.as_expr().unwrap()].kind
        );
    };
    assert_eq!(index, 0);
    let ExprKind::EnumConstruct {
        case_index,
        case_name,
        ..
    } = &body.exprs[right.as_expr().unwrap()].kind
    else {
        panic!(
            "expected EnumConstruct on right, got {:?}",
            body.exprs[right.as_expr().unwrap()].kind
        );
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
    let expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(
                variant_pat(scrut_ty, "Some", vec![wildcard_pat()], TypeTable::I32),
                bool_lit(true),
            ),
            arm(wildcard_pat(), bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn reduce_local_leaves_match_with_guard_alone() {
    // A guarded arm forces the fallthrough to depend on the guard's
    // runtime value; collapsing to a discriminator test would lose
    // that gate. Stay structurally intact.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm_with_guard(
                enum_pat(scrut_ty, "Case", 1),
                local_expr(2, TypeTable::BOOL),
                bool_lit(true),
            ),
            arm(wildcard_pat(), bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
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
    let expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(enum_pat(scrut_ty, "A", 0), bool_lit(true)),
            arm(enum_pat(scrut_ty, "B", 1), bool_lit(true)),
            arm(wildcard_pat(), bool_lit(false)),
        ],
        TypeTable::BOOL,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn reduce_local_leaves_match_with_non_bool_body_alone() {
    // The rule requires both arm bodies to be bool literals. An int
    // body falls through to the all-arms-equal collapse (which doesn't
    // match since the bodies are distinct) and the match stays put.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let scrut_ty = TypeTable::I32;
    let expr = match_expr(
        local_expr(0, scrut_ty),
        vec![
            arm(
                enum_pat(scrut_ty, "Case", 0),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn reduce_local_collapses_equal_arm_match_to_literal() {
    // Non-const speculatable scrutinee with all arms producing the same Const
    // collapses to that constant. The collapse promotes through the engine sink
    // in production; the value is observable here as the joined arm lattice.
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(wildcard_pat(), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    expect_int(&expr, 7, PrimitiveType::I32);
}

#[test]
fn reduce_local_leaves_unequal_arm_match_alone() {
    // Different Const arms under a non-const scrutinee: the match is
    // not rewritten.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(1, TypeTable::I32, "1")),
            arm(wildcard_pat(), int_lit(2, TypeTable::I32, "2")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn reduce_local_recurses_into_match_arm_body() {
    // The driver path enters `reduce` (not `reduce_local_in_body`) which uses
    // `reduce_in_place` to recurse into children. The arm body
    // `1 + 2` should fold to `3` even when the surrounding match
    // doesn't itself collapse (here the arm body's reduction is
    // observable as the engine's lattice value).
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let folded = match_expr(
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
    );
    let (body, e) = reduce_with(&mut interp, &folded);
    // After reduce: the match collapsed to Block([Expr(1 + 2)]). The arm body's
    // `1 + 2` is folded by the bottom-up walk but, under the scratch `BodySink`,
    // left in its operand slot — the fold is observable as the tail's lattice.
    let ExprKind::Block(blk) = body.exprs[e].kind else {
        panic!("expected Block, got {:?}", body.exprs[e].kind);
    };
    let s0 = body.blocks[blk].stmts[0];
    let StmtKind::Expr(tail) = body.stmts[s0].kind else {
        panic!("expected Expr stmt");
    };
    assert_eq!(
        interp.operand_to_lattice(&body, tail).as_const(),
        Some(Value::Int {
            value: 3,
            prim: PrimitiveType::I32
        })
    );
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
                constant_value_pat(int_lit(42, TypeTable::I32, "42")),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
                constant_value_pat(int_lit(99, TypeTable::I32, "99")),
                int_lit(1, TypeTable::I32, "1"),
            ),
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
    let expr = match_expr(
        int_lit(42, TypeTable::I32, "42"),
        vec![arm(
            constant_value_pat(local_expr(0, TypeTable::I32)),
            int_lit(1, TypeTable::I32, "1"),
        )],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

#[test]
fn match_or_pattern_no_match_no_unknowns_is_definite_no() {
    // Or-pattern with all definite-No alternatives reports No, so
    // a wildcard later catches the scrut. With const scrut == 99
    // and `Or([1, 2])` arm, the engine drops the Or arm and picks
    // the wildcard — `reduce_local_in_body` rewrites the match to the
    // wildcard's body block.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        int_lit(99, TypeTable::I32, "99"),
        vec![
            arm(
                or_pat(vec![lit_pat_i128(1), lit_pat_i128(2)]),
                int_lit(10, TypeTable::I32, "10"),
            ),
            arm(wildcard_pat(), int_lit(20, TypeTable::I32, "20")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(changed);
    let ExprKind::Block(blk) = body.exprs[e].kind else {
        panic!("expected Block");
    };
    let s0 = body.blocks[blk].stmts[0];
    let StmtKind::Expr(tail) = body.stmts[s0].kind else {
        panic!("expected Expr stmt");
    };
    assert_eq!(op_int(&body, tail), 20);
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
            arm(wildcard_pat(), int_lit(0, TypeTable::I32, "0")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
            arm(wildcard_pat(), int_lit(30, TypeTable::I32, "30")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut interp, &expr),
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
                or_pat(vec![lit_pat_i128(1), lit_pat_i128(2)]),
                int_lit(10, TypeTable::I32, "10"),
            ),
            arm(lit_pat_i128(3), int_lit(20, TypeTable::I32, "20")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
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
// fallback for unmatched scrutinee values. Wado's elaborator enforces
// match exhaustiveness for `bool` / `enum` / `variant` / range-covered
// `int`, but explicitly skips it for `struct` / `string` / `tuple` / …
// The engine must not collapse a non-exhaustive match to a literal
// (it would erase the trap), nor return a `Const(_)` lattice for one
// (a downstream `if` collapse could erase the trap on its behalf).
// ──────────────────────────────────────────────────────────────────────────────

fn binding_pat(name: &str, local_index: u32, type_id: TypeId) -> PatBuild {
    let name = name.to_string();
    Rc::new(move |b| {
        pp(
            b,
            PatKind::Binding {
                name: name.clone(),
                local_index,
                type_id,
            },
        )
    })
}

fn enum_pat(enum_type: TypeId, case_name: &str, case_index: u32) -> PatBuild {
    let case_name = case_name.to_string();
    Rc::new(move |b| {
        pp(
            b,
            PatKind::Enum {
                enum_type,
                case_name: case_name.clone(),
                case_index,
            },
        )
    })
}

fn variant_pat(
    enum_type: TypeId,
    variant_name: &str,
    bindings: Vec<PatBuild>,
    payload_type: TypeId,
) -> PatBuild {
    let variant_name = variant_name.to_string();
    Rc::new(move |b| {
        let bindings = bindings.iter().map(|p| p(b)).collect();
        pp(
            b,
            PatKind::Variant {
                enum_type,
                variant_name: variant_name.clone(),
                bindings,
                payload_type,
            },
        )
    })
}

fn constant_value_pat(expr: Build) -> PatBuild {
    Rc::new(move |b| {
        let e = expr(b);
        pp(b, PatKind::ConstantValue { expr: e })
    })
}

fn or_pat(alts: Vec<PatBuild>) -> PatBuild {
    Rc::new(move |b| {
        let alts = alts.iter().map(|p| p(b)).collect();
        pp(b, PatKind::Or(alts))
    })
}

fn tuple_pat(elems: Vec<PatBuild>, has_rest: bool) -> PatBuild {
    Rc::new(move |b| {
        let elems = elems.iter().map(|p| p(b)).collect();
        pp(b, PatKind::Tuple(elems, has_rest))
    })
}

#[test]
fn match_nonconst_scrut_non_exhaustive_all_arms_equal_does_not_rewrite() {
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(lit_pat_i128(2), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
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
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::NonConst,
    );
}

#[test]
fn match_nonconst_scrut_exhaustive_wildcard_collapses() {
    // Sanity: with an unguarded wildcard the match IS exhaustive, so the gate
    // lets the all-arms-equal collapse fire — the value joins to Const(7).
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm(wildcard_pat(), int_lit(7, TypeTable::I32, "7")),
        ],
        TypeTable::I32,
    );
    expect_int(&expr, 7, PrimitiveType::I32);
}

#[test]
fn match_const_scrut_only_unknown_arms_lattice_is_unevaluated() {
    // Const scrutinee, but the only arm has an Unknown pattern (Tuple here,
    // since Phase A doesn't model tuple patterns). The promoted-constant
    // scrutinee is handled structurally by the flow-fold visitor, not the
    // lattice, and the unmodeled pattern blocks the structural collapse — so
    // the lattice is `Unevaluated`. Crucially it must *not* be `Const(99)`: the
    // match may fall through to the trap, so the body is never folded in.
    let table = TypeTable::new();
    let expr = match_expr(
        int_lit(1, TypeTable::I32, "1"),
        vec![arm(
            tuple_pat(vec![], false),
            int_lit(99, TypeTable::I32, "99"),
        )],
        TypeTable::I32,
    );
    let lat = reduce_lat(&mut Interpreter::new(&table), &expr);
    assert_eq!(lat, Lattice::Unevaluated);
    assert_eq!(lat.as_const(), None);
}

#[test]
fn match_nonconst_scrut_or_pattern_with_embedded_wildcard_is_exhaustive() {
    // `Or([1, _])` contains an unguarded wildcard alternative; the engine
    // should recognize it as a catch-all and let the collapse fire.
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![arm(
            or_pat(vec![lit_pat_i128(1), wildcard_pat()]),
            int_lit(7, TypeTable::I32, "7"),
        )],
        TypeTable::I32,
    );
    expect_int(&expr, 7, PrimitiveType::I32);
}

#[test]
fn match_nonconst_scrut_binding_pattern_counts_as_exhaustive() {
    // A `Binding` pattern always matches and captures the value — a catch-all
    // by another name.
    let expr = match_expr(
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
    expect_int(&expr, 7, PrimitiveType::I32);
}

#[test]
fn match_nonconst_scrut_guarded_wildcard_does_not_count_as_exhaustive() {
    // A guarded catch-all is NOT exhaustive — if the guard fails,
    // control falls through to the implicit trap.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let expr = match_expr(
        local_expr(0, TypeTable::I32),
        vec![
            arm(lit_pat_i128(1), int_lit(7, TypeTable::I32, "7")),
            arm_with_guard(
                wildcard_pat(),
                local_expr(1, TypeTable::BOOL),
                int_lit(7, TypeTable::I32, "7"),
            ),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Match { .. }));
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure call inlining (try_call_fold)
// ──────────────────────────────────────────────────────────────────────────────

/// Build a minimal `NirFunction` with the supplied signature and a
/// single-statement body. `body_stmt` is normally `StmtKind::Return`
/// or `StmtKind::Expr`. The function is module-public, non-async,
/// non-CM, non-method, non-generic — i.e. CTFE-eligible by default.
/// A deferred statement builder.
type StmtBuild = Rc<dyn Fn(&mut Body) -> StmtId>;

fn ps(body: &mut Body, kind: StmtKind) -> StmtId {
    body.stmts.push(StmtNode {
        kind,
        span: Span::default(),
    })
}

fn return_stmt(value: Build) -> StmtBuild {
    Rc::new(move |b| {
        let v = value(b);
        ps(b, StmtKind::Return { value: Some(v) })
    })
}

fn expr_stmt_b(e: Build) -> StmtBuild {
    Rc::new(move |b| {
        let e = e(b);
        ps(b, StmtKind::Expr(e))
    })
}

fn return_none() -> StmtBuild {
    Rc::new(|b| ps(b, StmtKind::Return { value: None }))
}

fn let_stmt_b(name: &str, local_index: u32, type_id: TypeId, value: Build) -> StmtBuild {
    let name = name.to_string();
    Rc::new(move |b| {
        let value = value(b);
        ps(
            b,
            StmtKind::Let {
                name: name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: false,
            },
        )
    })
}

fn let_mut_stmt_b(name: &str, local_index: u32, type_id: TypeId, value: Build) -> StmtBuild {
    let name = name.to_string();
    Rc::new(move |b| {
        let value = value(b);
        ps(
            b,
            StmtKind::Let {
                name: name.clone(),
                local_index,
                is_mut: true,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: false,
            },
        )
    })
}

/// `local = value` as an expression statement.
fn assign_local_stmt_b(local_index: u32, type_id: TypeId, value: Build) -> StmtBuild {
    Rc::new(move |b| {
        let target = pe(
            b,
            ExprKind::Local {
                index: local_index,
                name: String::new(),
            },
            type_id,
        );
        let value = value(b);
        let assign = pe(b, ExprKind::Assign { target, value }, TypeTable::UNIT);
        ps(b, StmtKind::Expr(Operand::Expr(assign)))
    })
}

/// `target = value` as an expression statement, for a target that is a
/// projection rather than a bare local.
fn assign_stmt_b(target: Build, value: Build) -> StmtBuild {
    Rc::new(move |b| {
        let target = target(b)
            .as_expr()
            .expect("a store target is a skeleton expression");
        let value = value(b);
        let assign = pe(b, ExprKind::Assign { target, value }, TypeTable::UNIT);
        ps(b, StmtKind::Expr(Operand::Expr(assign)))
    })
}

fn index_expr(receiver: Build, index: Build, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let expr = receiver(b);
        let index = index(b);
        Operand::Expr(pe(b, ExprKind::Index { expr, index }, type_id))
    })
}

fn block_of(b: &mut Body, stmts: &[StmtBuild]) -> BlockId {
    let ids: Vec<StmtId> = stmts.iter().map(|s| s(b)).collect();
    b.blocks.push(BlockNode {
        stmts: ids,
        span: Span::default(),
    })
}

fn if_stmt_b(
    condition: Build,
    then_stmts: Vec<StmtBuild>,
    else_stmts: Vec<StmtBuild>,
) -> StmtBuild {
    Rc::new(move |b| {
        let condition = condition(b);
        let then_block = block_of(b, &then_stmts);
        let else_block = (!else_stmts.is_empty()).then(|| block_of(b, &else_stmts));
        ps(
            b,
            StmtKind::If {
                condition,
                then_block,
                else_block,
            },
        )
    })
}

fn loop_stmt_b(stmts: Vec<StmtBuild>) -> StmtBuild {
    Rc::new(move |b| {
        let body = block_of(b, &stmts);
        ps(b, StmtKind::Loop { body })
    })
}

fn array_literal(type_id: TypeId, elements: Vec<Build>) -> Build {
    Rc::new(move |b| {
        let elements = elements.iter().map(|e| e(b)).collect();
        Operand::Expr(pe(b, ExprKind::ArrayLiteral { elements }, type_id))
    })
}

fn packed_array(bytes: Vec<u8>, type_id: TypeId) -> Build {
    Rc::new(move |b| Operand::Expr(pe(b, ExprKind::PackedArray(bytes.clone()), type_id)))
}

fn shared_ref(inner: Build, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let inner = inner(b);
        Operand::Expr(pe(
            b,
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            },
            type_id,
        ))
    })
}

fn mut_ref(inner: Build, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let inner = inner(b);
        Operand::Expr(pe(
            b,
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            },
            type_id,
        ))
    })
}

/// `array_set(&mut place, i, v)`: the first argument is the mutable one.
fn seq_write_call(func_id: wado_compiler::nir::FuncId, args: Vec<Build>, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let args = args
            .iter()
            .enumerate()
            .map(|(i, a)| wado_compiler::nir_arena::ArenaCallArg {
                expr: a(b),
                is_mut: i == 0,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::Call {
                func_id,
                type_args: Vec::new(),
                args,
                has_receiver: false,
            },
            type_id,
        ))
    })
}

/// A call to a synthetic builtin id, paired with the map that classifies it.
fn ctfe_builtin_call(
    func_id: wado_compiler::nir::FuncId,
    args: Vec<Build>,
    type_id: TypeId,
) -> Build {
    Rc::new(move |b| {
        let args = args
            .iter()
            .map(|a| wado_compiler::nir_arena::ArenaCallArg {
                expr: a(b),
                is_mut: false,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::Call {
                func_id,
                type_args: Vec::new(),
                args,
                has_receiver: false,
            },
            type_id,
        ))
    })
}

fn ctfe_builtin_map(func_id: wado_compiler::nir::FuncId, builtin: CtfeBuiltin) -> CtfeBuiltinMap {
    let mut map = CtfeBuiltinMap::default();
    map.insert(func_id, builtin);
    map
}

#[test]
fn array_literal_reduces_to_the_container_it_denotes() {
    // An array literal lowers to `{ repr: array.new_fixed, used: N }`, so it
    // denotes the whole `List` — not the backing array.
    let table = TypeTable::new();
    let lit = array_literal(
        TypeTable::I32,
        vec![
            int_lit(10, TypeTable::I32, "10"),
            int_lit(20, TypeTable::I32, "20"),
        ],
    );
    let Lattice::Const(v) = reduce_lat(&mut Interpreter::new(&table), &lit) else {
        panic!("a constant array literal is a constant container");
    };
    assert_eq!(
        v.field(SeqField::Len.index()).and_then(Value::as_int),
        Some((2, PrimitiveType::I32))
    );
    let backing = v.field(SeqField::Backing.index()).expect("a backing array");
    assert_eq!(backing.seq_len(), Some(2));
    assert_eq!(
        backing.element(1).and_then(Value::as_int).map(|(n, _)| n),
        Some(20)
    );
}

fn container_lit(type_id: TypeId, backing: Build, used: u64) -> Build {
    Rc::new(move |b| {
        let backing = backing(b);
        let used = int_lit(used, TypeTable::I32, "len")(b);
        Operand::Expr(pe(
            b,
            ExprKind::StructLiteral {
                struct_type: type_id,
                struct_name: "String".to_string(),
                fields: vec![
                    ArenaStructField {
                        name: SeqField::Backing.field_name().to_string(),
                        value: backing,
                        field_index: SeqField::Backing.index(),
                    },
                    ArenaStructField {
                        name: SeqField::Len.field_name().to_string(),
                        value: used,
                        field_index: SeqField::Len.index(),
                    },
                ],
            },
            type_id,
        ))
    })
}

/// The shape the lower phase emits for a source string: a container struct
/// over a packed byte array plus its length.
fn seq_lit(type_id: TypeId, bytes: Vec<u8>) -> Build {
    let used = bytes.len() as u64;
    container_lit(type_id, packed_array(bytes, type_id), used)
}

/// Register the container items `materialize_seq_via` identifies by, and the
/// `Array<u8>` the literal it writes names. A bare table has neither, and the
/// compiler's own has both.
fn register_seq_containers(table: &mut TypeTable) {
    table.make_builtin_array(TypeTable::U8);
    for (item, name) in [
        (wado_compiler::compiler_item::CompilerItem::String, "String"),
        (wado_compiler::compiler_item::CompilerItem::List, "List"),
    ] {
        table
            .compiler_items_mut()
            .register(
                item,
                wado_compiler::compiler_item::Resolved::Struct {
                    module_source: ModuleSource::default(),
                    name: name.to_string(),
                },
            )
            .expect("a struct item takes a struct");
    }
}

#[test]
fn a_constant_string_call_result_becomes_a_literal() {
    // A CTFE call returning a `String` reduces to the container it denotes, and
    // the exit writes that container back as the literal the lower phase emits
    // for a source string — instead of discarding it for not being a scalar.
    let mut table = TypeTable::new();
    register_seq_containers(&mut table);
    let string_ty = table.make_struct("String".to_string(), ModuleSource::default());
    let greeting = make_pure_fn(
        "greeting",
        vec![],
        string_ty,
        return_stmt(seq_lit(string_ty, b"hi".to_vec())),
    );
    let callees = build_callee_map_test(std::slice::from_ref(&greeting));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let (changed, body, e) = reduce_local_into(&mut interp, &call_expr(&greeting, vec![]));
    assert!(changed, "a constant String call folds");
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[e].kind else {
        panic!("the folded call is the container literal");
    };
    let backing = fields
        .iter()
        .find(|f| f.field_index == SeqField::Backing.index())
        .and_then(|f| f.value.as_expr())
        .expect("a backing operand");
    assert!(matches!(&body.exprs[backing].kind, ExprKind::PackedArray(b) if b == b"hi"));
    assert_eq!(
        fields
            .iter()
            .find(|f| f.field_index == SeqField::Len.index())
            .map(|f| op_int(&body, f.value)),
        Some(2),
    );
}

#[test]
fn a_container_still_copying_its_contents_becomes_a_literal_once() {
    // `String { repr: array_clone_prefix(&"hi", 2), used: 2 }` — the shape a
    // copy of a constant leaves behind. Writing the literal over it drops an
    // allocation and a copy per evaluation.
    //
    // And it must happen exactly once: the rewrite produces a container
    // literal, which is the same node kind it admits, so a second pass finding
    // more to do would keep the fixed-point loop reporting changes forever.
    let mut table = TypeTable::new();
    register_seq_containers(&mut table);
    let string_ty = table.make_struct("String".to_string(), ModuleSource::default());
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let clone_id = next_test_func_id();
    let builtins = ctfe_builtin_map(clone_id, CtfeBuiltin::ArrayClonePrefix);

    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);

    let copied = container_lit(
        string_ty,
        ctfe_builtin_call(
            clone_id,
            vec![
                shared_ref(packed_array(b"hi".to_vec(), array_ty), array_ty),
                int_lit(2, TypeTable::I32, "2"),
            ],
            array_ty,
        ),
        2,
    );
    let (changed, mut body, e) = reduce_local_into(&mut interp, &copied);
    assert!(changed, "the copy folds into the container literal");
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[e].kind else {
        panic!("the container survives as a literal");
    };
    let backing = fields
        .iter()
        .find(|f| f.field_index == SeqField::Backing.index())
        .and_then(|f| f.value.as_expr())
        .expect("a backing operand");
    assert!(matches!(&body.exprs[backing].kind, ExprKind::PackedArray(b) if b == b"hi"));
    assert!(
        !interp.reduce_local_in_body(&mut body, e),
        "the literal it wrote is left alone",
    );
}

#[test]
fn a_write_does_not_reach_a_value_copied_out_before_it() {
    // `let d = c` is a copy under Wado's value semantics, and the engine
    // shares one backing between the two until something writes. That write
    // has to fork the backing: writing where it lies would reach through every
    // copy taken of it.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let backing_of = move |local: u32| {
        field_access(
            local_expr(local, list_ty),
            SeqField::Backing.index(),
            "repr",
            list_ty,
        )
    };
    // fn f() { let mut c = [1, 2]; let d = c; c.repr[0] = 9; return d.repr[0]; }
    let copy_then_write = make_multi_stmt_fn(
        "copy_then_write",
        vec![],
        TypeTable::U8,
        &[("c", list_ty, true), ("d", list_ty, false)],
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![1, 2])),
            let_stmt_b("d", 1, list_ty, local_expr(0, list_ty)),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(backing_of(0), list_ty),
                    int_lit(0, TypeTable::I32, "0"),
                    int_lit(9, TypeTable::U8, "9"),
                ],
                TypeTable::UNIT,
            )),
            return_stmt(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(backing_of(1), list_ty),
                    int_lit(0, TypeTable::I32, "0"),
                ],
                TypeTable::U8,
            )),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&copy_then_write));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&copy_then_write, vec![])),
        Some(Value::Int {
            value: 1,
            prim: PrimitiveType::U8,
        }),
        "the copy must still hold what it was given",
    );
}

#[test]
fn a_write_through_a_frame_owned_place_is_read_back() {
    // `array_set` through a `&mut` place the frame built updates the container,
    // so a later read of that element sees the written value rather than
    // abandoning the evaluation.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let backing = || {
        field_access(
            local_expr(0, list_ty),
            SeqField::Backing.index(),
            "repr",
            list_ty,
        )
    };
    let set_then_get = make_pure_fn_stmts(
        "set_then_get",
        vec![],
        TypeTable::U8,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(backing(), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(7, TypeTable::U8, "7"),
                ],
                TypeTable::UNIT,
            )),
            return_stmt(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(backing(), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&set_then_get));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&set_then_get, vec![])),
        Some(Value::Int {
            value: 7,
            prim: PrimitiveType::U8,
        }),
    );
}

#[test]
fn a_write_through_a_place_the_frame_does_not_own_is_refused() {
    // The same write rooted at a global: the frame holds no value for that
    // root, so the statement abandons the evaluation instead of stepping past a
    // write it did not apply — the constant the body would return does not come
    // out either.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);

    let write_global = make_pure_fn_stmts(
        "write_global",
        vec![],
        TypeTable::U8,
        vec![
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(
                        field_access(
                            global_get(ModuleSource::default(), "BUF", list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(7, TypeTable::U8, "7"),
                ],
                TypeTable::UNIT,
            )),
            return_stmt(int_lit(7, TypeTable::U8, "7")),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&write_global));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&write_global, vec![])),
        None
    );
}

#[test]
fn a_field_store_through_a_frame_owned_place_is_read_back() {
    // A store into a field of a container the frame built updates the frame's
    // value for it, so a later read of that field sees what was written.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let used = || {
        field_access(
            local_expr(0, list_ty),
            SeqField::Len.index(),
            "used",
            TypeTable::I32,
        )
    };
    let store_then_read = make_pure_fn_stmts(
        "store_then_read",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            assign_stmt_b(used(), int_lit(1, TypeTable::I32, "1")),
            return_stmt(used()),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&store_then_read));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&store_then_read, vec![])),
        Some(Value::Int {
            value: 1,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn a_store_through_a_projection_that_is_not_a_place_is_refused() {
    // An element position is not a field path, so the target names no place the
    // frame can update — the write is refused rather than stepped past.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let store_through_index = make_pure_fn_stmts(
        "store_through_index",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            assign_stmt_b(
                field_access(
                    index_expr(
                        local_expr(0, list_ty),
                        int_lit(0, TypeTable::I32, "0"),
                        list_ty,
                    ),
                    SeqField::Len.index(),
                    "used",
                    TypeTable::I32,
                ),
                int_lit(1, TypeTable::I32, "1"),
            ),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&store_through_index));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&store_through_index, vec![])),
        None,
    );
}

/// A container over a freshly allocated backing array of `capacity` bytes,
/// holding `used` of them — the shape `List::with_capacity` returns.
fn allocated_container(
    list_ty: TypeId,
    array_ty: TypeId,
    new_id: wado_compiler::nir::FuncId,
    capacity: u64,
    used: u64,
) -> Build {
    struct_lit(
        list_ty,
        vec![
            (
                SeqField::Backing.index(),
                "repr",
                ctfe_builtin_call(
                    new_id,
                    vec![int_lit(capacity, TypeTable::I32, "capacity")],
                    array_ty,
                ),
            ),
            (
                SeqField::Len.index(),
                "used",
                int_lit(used, TypeTable::I32, "used"),
            ),
        ],
    )
}

#[test]
fn array_new_denotes_a_zero_filled_sequence() {
    // A fresh allocation is every element's default, so reading one back gives
    // zero instead of abandoning the evaluation.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let new_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(new_id, CtfeBuiltin::ArrayNew);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let new_then_read = make_pure_fn_stmts(
        "new_then_read",
        vec![],
        TypeTable::U8,
        vec![
            let_stmt_b(
                "c",
                0,
                list_ty,
                allocated_container(list_ty, array_ty, new_id, 3, 3),
            ),
            return_stmt(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(
                        field_access(
                            local_expr(0, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            array_ty,
                        ),
                        array_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&new_then_read));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&new_then_read, vec![])),
        Some(Value::Int {
            value: 0,
            prim: PrimitiveType::U8,
        }),
    );
}

#[test]
fn an_allocation_past_the_element_cap_is_not_a_sequence() {
    // Building the value would walk every element, so past the cap the
    // allocation is simply not a constant here and the call stays.
    let mut table = TypeTable::new();
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let new_id = next_test_func_id();
    let builtins = ctfe_builtin_map(new_id, CtfeBuiltin::ArrayNew);

    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let oversized = ctfe_builtin_call(
        new_id,
        vec![int_lit(
            MAX_SEQ_ELEMENTS as u64 + 1,
            TypeTable::I32,
            "capacity",
        )],
        array_ty,
    );

    assert_eq!(reduce_lat(&mut interp, &oversized), Lattice::Unevaluated);
}

#[test]
fn a_container_the_frame_never_filled_stays_an_allocation() {
    // Materializing it would write the reservation back as an empty literal,
    // trading a capacity the source asked for against nothing.
    let mut table = TypeTable::new();
    register_seq_containers(&mut table);
    let string_ty = table.make_struct("String".to_string(), ModuleSource::default());
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let new_id = next_test_func_id();
    let builtins = ctfe_builtin_map(new_id, CtfeBuiltin::ArrayNew);

    let with_capacity = make_pure_fn(
        "with_capacity",
        vec![],
        string_ty,
        return_stmt(allocated_container(string_ty, array_ty, new_id, 8, 0)),
    );
    let callees = build_callee_map_test(std::slice::from_ref(&with_capacity));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    let (changed, body, e) = reduce_local_into(&mut interp, &call_expr(&with_capacity, vec![]));
    assert!(!changed, "a reservation is not written back as a literal");
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn a_copy_into_a_frame_owned_place_splices_the_source() {
    // `array_copy` at statement position lands in the frame's container, so a
    // later read of a copied element sees the source's byte.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let copy_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(copy_id, CtfeBuiltin::ArrayCopy);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let backing = move || {
        field_access(
            local_expr(0, list_ty),
            SeqField::Backing.index(),
            "repr",
            array_ty,
        )
    };
    let copy_then_read = make_pure_fn_stmts(
        "copy_then_read",
        vec![],
        TypeTable::U8,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0, 0, 0])),
            expr_stmt_b(seq_write_call(
                copy_id,
                vec![
                    mut_ref(backing(), array_ty),
                    int_lit(1, TypeTable::I32, "1"),
                    shared_ref(packed_array(vec![7, 8], array_ty), array_ty),
                    int_lit(0, TypeTable::I32, "0"),
                    int_lit(2, TypeTable::I32, "2"),
                ],
                TypeTable::UNIT,
            )),
            return_stmt(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(backing(), array_ty),
                    int_lit(2, TypeTable::I32, "2"),
                ],
                TypeTable::U8,
            )),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&copy_then_read));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&copy_then_read, vec![])),
        Some(Value::Int {
            value: 8,
            prim: PrimitiveType::U8,
        }),
    );
}

#[test]
fn a_copy_past_the_end_of_the_destination_is_refused() {
    // The run traps at run time, so the evaluation is abandoned rather than
    // producing a container the write never made.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let array_ty = table.make_builtin_array(TypeTable::U8);
    let copy_id = next_test_func_id();
    let builtins = ctfe_builtin_map(copy_id, CtfeBuiltin::ArrayCopy);

    let overrun = make_pure_fn_stmts(
        "overrun",
        vec![],
        TypeTable::U8,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            expr_stmt_b(seq_write_call(
                copy_id,
                vec![
                    mut_ref(
                        field_access(
                            local_expr(0, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            array_ty,
                        ),
                        array_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                    shared_ref(packed_array(vec![7, 8], array_ty), array_ty),
                    int_lit(0, TypeTable::I32, "0"),
                    int_lit(2, TypeTable::I32, "2"),
                ],
                TypeTable::UNIT,
            )),
            return_stmt(int_lit(7, TypeTable::U8, "7")),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&overrun));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(flow_fold(&mut interp, &call_expr(&overrun, vec![])), None);
}

/// `c.used` for the container held in local 0.
fn used_of_local(list_ty: TypeId) -> Build {
    field_access(
        local_expr(0, list_ty),
        SeqField::Len.index(),
        "used",
        TypeTable::I32,
    )
}

/// A statement incrementing the container in local 0.
fn bump_used(list_ty: TypeId) -> StmtBuild {
    assign_stmt_b(
        used_of_local(list_ty),
        binary(
            NirBinaryOp::Add,
            used_of_local(list_ty),
            int_lit(1, TypeTable::I32, "1"),
            TypeTable::I32,
        ),
    )
}

#[test]
fn a_call_writing_through_a_mut_ref_updates_the_caller_place() {
    // What the run produces is the caller's place: the callee returns nothing,
    // so a run whose writes went nowhere would be a run for nothing.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump = with_mut_ref_params(
        make_pure_fn_stmts(
            "bump",
            vec![("c", list_ty)],
            TypeTable::UNIT,
            vec![bump_used(list_ty)],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            expr_stmt_b(call_expr_args(
                &bump,
                vec![(mut_ref(local_expr(0, list_ty), list_ty), true)],
            )),
            return_stmt(used_of_local(list_ty)),
        ],
    );
    let callees = build_callee_map_test(&[bump, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        Some(Value::Int {
            value: 3,
            prim: PrimitiveType::I32,
        }),
    );
}

/// `fn add(c: &mut List<u8>, n: <second_param_ty>) { c.used = c.used + <n as i32>; }`
/// paired with a caller passing `&mut c` and a second argument built over the
/// same local, returning `c.used`.
fn add_through_mut_ref_and_second_arg(
    list_ty: TypeId,
    second_param_ty: TypeId,
    second_arg: Build,
    addend_in_callee: Build,
) -> (NirFunction, NirFunction) {
    let add = with_mut_ref_params(
        make_pure_fn_stmts(
            "add",
            vec![("c", list_ty), ("n", second_param_ty)],
            TypeTable::UNIT,
            vec![assign_stmt_b(
                used_of_local(list_ty),
                binary(
                    NirBinaryOp::Add,
                    used_of_local(list_ty),
                    addend_in_callee,
                    TypeTable::I32,
                ),
            )],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            expr_stmt_b(call_expr_args(
                &add,
                vec![
                    (mut_ref(local_expr(0, list_ty), list_ty), true),
                    (second_arg, false),
                ],
            )),
            return_stmt(used_of_local(list_ty)),
        ],
    );
    (add, caller)
}

#[test]
fn a_second_argument_naming_a_mut_ref_target_declines_the_call() {
    // A second argument binds its own snapshot of storage the callee writes
    // through `&mut`, so running the call would read a value the program never
    // had. Wado has no borrow checker, so this is ordinary source: decline
    // rather than mis-run.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let list_ref_ty = table.make_ref(list_ty);

    let (add, caller) = add_through_mut_ref_and_second_arg(
        list_ty,
        list_ref_ty,
        shared_ref(local_expr(0, list_ty), list_ref_ty),
        field_access(
            local_expr(1, list_ref_ty),
            SeqField::Len.index(),
            "used",
            TypeTable::I32,
        ),
    );
    let callees = build_callee_map_test(&[add, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(flow_fold(&mut interp, &call_expr(&caller, vec![])), None);
}

#[test]
fn a_method_call_writes_back_through_its_receiver() {
    // A method names its receiver directly rather than through `&mut`, and
    // what fills a container is always a method.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump = with_mut_ref_params(
        make_pure_fn_stmts(
            "List<u8>::bump",
            vec![("self", list_ty)],
            TypeTable::UNIT,
            vec![bump_used(list_ty)],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            expr_stmt_b(method_call_expr(&bump, local_expr(0, list_ty), Vec::new())),
            return_stmt(used_of_local(list_ty)),
        ],
    );
    let callees = build_callee_map_test(&[bump, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        Some(Value::Int {
            value: 3,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn a_mutating_call_outside_statement_position_is_not_run() {
    // The lattice projection is re-entrant, so a write applied there could
    // land twice. Only the executor runs a mutating call, and only where it
    // runs exactly once.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump_get = with_mut_ref_params(
        make_pure_fn_stmts(
            "bump_get",
            vec![("c", list_ty)],
            TypeTable::I32,
            vec![bump_used(list_ty), return_stmt(used_of_local(list_ty))],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            return_stmt(binary(
                NirBinaryOp::Add,
                call_expr_args(
                    &bump_get,
                    vec![(mut_ref(local_expr(0, list_ty), list_ty), true)],
                ),
                used_of_local(list_ty),
                TypeTable::I32,
            )),
        ],
    );
    let callees = build_callee_map_test(&[bump_get, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(flow_fold(&mut interp, &call_expr(&caller, vec![])), None);
}

#[test]
fn a_mutating_call_bound_by_a_let_writes_back() {
    // A `let` runs its value exactly once, so a call that both returns and
    // writes is at home there.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump_get = with_mut_ref_params(
        make_pure_fn_stmts(
            "bump_get",
            vec![("c", list_ty)],
            TypeTable::I32,
            vec![bump_used(list_ty), return_stmt(used_of_local(list_ty))],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            let_stmt_b(
                "a",
                1,
                TypeTable::I32,
                call_expr_args(
                    &bump_get,
                    vec![(mut_ref(local_expr(0, list_ty), list_ty), true)],
                ),
            ),
            return_stmt(binary(
                NirBinaryOp::Add,
                binary(
                    NirBinaryOp::Mul,
                    local_expr(1, TypeTable::I32),
                    int_lit(10, TypeTable::I32, "10"),
                    TypeTable::I32,
                ),
                used_of_local(list_ty),
                TypeTable::I32,
            )),
        ],
    );
    let callees = build_callee_map_test(&[bump_get, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        Some(Value::Int {
            value: 33,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn a_run_that_bails_part_way_writes_nothing() {
    // The callee's first statement lands, its second is one the frame cannot
    // perform. Stepping past that with the first write applied would hand the
    // caller a container the callee never produced.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let half = with_mut_ref_params(
        make_pure_fn_stmts(
            "half",
            vec![("c", list_ty)],
            TypeTable::UNIT,
            vec![
                bump_used(list_ty),
                assign_stmt_b(
                    field_access(
                        index_expr(
                            local_expr(0, list_ty),
                            int_lit(0, TypeTable::I32, "0"),
                            list_ty,
                        ),
                        SeqField::Len.index(),
                        "used",
                        TypeTable::I32,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                ),
            ],
        ),
        &[0],
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            expr_stmt_b(call_expr_args(
                &half,
                vec![(mut_ref(local_expr(0, list_ty), list_ty), true)],
            )),
            return_stmt(used_of_local(list_ty)),
        ],
    );
    let callees = build_callee_map_test(&[half, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(flow_fold(&mut interp, &call_expr(&caller, vec![])), None);
}

#[test]
fn a_stores_clause_does_not_stop_a_run() {
    // `stores` constrains a *reference* the callee keeps, and the engine has
    // no references: an argument reduces to its referent's value, and a
    // referent it can bind is one nothing in the frame can go on to change.
    let table = TypeTable::new();
    let mut keeper = make_pure_fn(
        "keep",
        vec![("value", TypeTable::I32)],
        TypeTable::I32,
        return_stmt(local_expr(0, TypeTable::I32)),
    );
    keeper.stores = vec!["value".to_string()];
    let callees = build_callee_map_test(std::slice::from_ref(&keeper));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(
            &mut interp,
            &call_expr(&keeper, vec![int_lit(7, TypeTable::I32, "7")])
        ),
        Some(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn an_argument_still_spelled_as_arithmetic_binds() {
    // A call runs before the walk has folded its arguments, so binding a
    // parameter has to fold what it is handed rather than read a literal.
    let table = TypeTable::new();
    let plus_one = make_pure_fn(
        "plus_one",
        vec![("n", TypeTable::I32)],
        TypeTable::I32,
        return_stmt(binary(
            NirBinaryOp::Add,
            local_expr(0, TypeTable::I32),
            int_lit(1, TypeTable::I32, "1"),
            TypeTable::I32,
        )),
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_stmt_b("k", 0, TypeTable::I32, int_lit(3, TypeTable::I32, "3")),
            let_stmt_b(
                "a",
                1,
                TypeTable::I32,
                call_expr(
                    &plus_one,
                    vec![binary(
                        NirBinaryOp::Mul,
                        local_expr(0, TypeTable::I32),
                        int_lit(2, TypeTable::I32, "2"),
                        TypeTable::I32,
                    )],
                ),
            ),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    );
    let callees = build_callee_map_test(&[plus_one, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        Some(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn a_shared_receiver_leaves_the_container_trackable() {
    // Reading through `&self` cannot write, so a container stays trackable
    // across the very `len()` calls a caller reads it with — and `push` reads
    // its own capacity that way on every call.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump = with_mut_ref_params(
        make_pure_fn_stmts(
            "List<u8>::bump",
            vec![("self", list_ty)],
            TypeTable::UNIT,
            vec![bump_used(list_ty)],
        ),
        &[0],
    );
    let len = make_pure_fn(
        "List<u8>::len",
        vec![("self", list_ty)],
        TypeTable::I32,
        return_stmt(used_of_local(list_ty)),
    );
    let caller = make_pure_fn_stmts(
        "caller",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![5, 6])),
            expr_stmt_b(method_call_expr(&bump, local_expr(0, list_ty), Vec::new())),
            return_stmt(method_call_expr(&len, local_expr(0, list_ty), Vec::new())),
        ],
    );
    let callees = build_callee_map_test(&[bump, len, caller.clone()]);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        Some(Value::Int {
            value: 3,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn select_picks_the_arm_its_condition_names() {
    // A comparison collapses to branchless `select` before niri sees it, and
    // that is what stands between a container and the capacity it grows to.
    let table = TypeTable::new();
    let select_id = next_test_func_id();
    let builtins = ctfe_builtin_map(select_id, CtfeBuiltin::Select);

    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);

    let pick = |condition: bool| {
        ctfe_builtin_call(
            select_id,
            vec![
                bool_lit(condition),
                int_lit(10, TypeTable::I32, "10"),
                int_lit(20, TypeTable::I32, "20"),
            ],
            TypeTable::I32,
        )
    };

    assert_eq!(
        reduce_lat(&mut interp, &pick(true)),
        Lattice::Const(Value::Int {
            value: 10,
            prim: PrimitiveType::I32,
        }),
    );
    // Each `pick` builds a fresh body whose arena reuses the same ids, so the
    // frame — fold memo included — must reset between them, as the driving
    // visitor resets it per function.
    interp.enter_function();
    assert_eq!(
        reduce_lat(&mut interp, &pick(false)),
        Lattice::Const(Value::Int {
            value: 20,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn a_branch_hint_is_stepped_past() {
    // `cold_path()` computes nothing, and it sits on the very path that grows
    // a container — a frame that could not step past it would stop one
    // statement before the growth it guards.
    let table = TypeTable::new();
    let hint_id = next_test_func_id();
    let builtins = ctfe_builtin_map(hint_id, CtfeBuiltin::ColdPath);

    let hinted = make_pure_fn_stmts(
        "hinted",
        vec![],
        TypeTable::I32,
        vec![
            expr_stmt_b(ctfe_builtin_call(hint_id, Vec::new(), TypeTable::UNIT)),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&hinted));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.with_ctfe_builtins(&builtins);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&hinted, vec![])),
        Some(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
    );
}

#[test]
fn packed_array_reduces_to_a_sequence_of_bytes() {
    let table = TypeTable::new();
    let lit = packed_array(b"hi".to_vec(), TypeTable::I32);
    let Lattice::Const(v) = reduce_lat(&mut Interpreter::new(&table), &lit) else {
        panic!("a byte-string literal is a sequence");
    };
    assert_eq!(v.seq_len(), Some(2));
    assert_eq!(
        v.element(0).and_then(Value::as_int),
        Some((u64::from(b'h'), PrimitiveType::U8))
    );
}

#[test]
fn a_sequence_over_the_cap_is_not_modelled() {
    let table = TypeTable::new();
    let big = packed_array(vec![0u8; MAX_SEQ_ELEMENTS + 1], TypeTable::I32);
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &big),
        Lattice::NonConst,
    );
    let at_cap = packed_array(vec![0u8; MAX_SEQ_ELEMENTS], TypeTable::I32);
    assert!(matches!(
        reduce_lat(&mut Interpreter::new(&table), &at_cap),
        Lattice::Const(_),
    ));
}

#[test]
fn array_get_folds_an_element() {
    let func_id = next_test_func_id();
    let map = ctfe_builtin_map(func_id, CtfeBuiltin::ArrayGet);
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&map);
    // `arr[i]` lowers to the builtin over the backing array projected out of
    // the container.
    let call = ctfe_builtin_call(
        func_id,
        vec![
            field_access(
                array_literal(
                    TypeTable::I32,
                    vec![
                        int_lit(10, TypeTable::I32, "10"),
                        int_lit(20, TypeTable::I32, "20"),
                        int_lit(30, TypeTable::I32, "30"),
                    ],
                ),
                SeqField::Backing.index(),
                "repr",
                TypeTable::I32,
            ),
            int_lit(2, TypeTable::I32, "2"),
        ],
        TypeTable::I32,
    );
    assert_eq!(flow_fold(&mut interp, &call), i32_of(30));
}

#[test]
fn array_get_reads_through_a_shared_reference() {
    let func_id = next_test_func_id();
    let map = ctfe_builtin_map(func_id, CtfeBuiltin::ArrayGet);
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&map);
    let call = ctfe_builtin_call(
        func_id,
        vec![
            shared_ref(
                packed_array(b"abc".to_vec(), TypeTable::I32),
                TypeTable::I32,
            ),
            int_lit(1, TypeTable::I32, "1"),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        flow_fold(&mut interp, &call),
        Some(Value::Int {
            value: u64::from(b'b'),
            prim: PrimitiveType::U8,
        }),
    );
}

#[test]
fn array_get_past_the_end_is_left_alone() {
    // Folding the read would delete the run-time trap.
    let func_id = next_test_func_id();
    let map = ctfe_builtin_map(func_id, CtfeBuiltin::ArrayGet);
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&map);
    let call = ctfe_builtin_call(
        func_id,
        vec![
            array_literal(TypeTable::I32, vec![int_lit(10, TypeTable::I32, "10")]),
            int_lit(5, TypeTable::I32, "5"),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &call);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn array_len_folds_to_the_element_count() {
    let func_id = next_test_func_id();
    let map = ctfe_builtin_map(func_id, CtfeBuiltin::ArrayLen);
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&map);
    let call = ctfe_builtin_call(
        func_id,
        vec![packed_array(b"hello".to_vec(), TypeTable::I32)],
        TypeTable::I32,
    );
    assert_eq!(flow_fold(&mut interp, &call), i32_of(5));
}

#[test]
fn a_sequence_without_the_builtin_map_stays_a_call() {
    let func_id = next_test_func_id();
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    let call = ctfe_builtin_call(
        func_id,
        vec![
            array_literal(TypeTable::I32, vec![int_lit(10, TypeTable::I32, "10")]),
            int_lit(0, TypeTable::I32, "0"),
        ],
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &call);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn a_shared_reference_reduces_to_what_it_points_at() {
    // A borrow is not an operation over a value. Evaluating it as one reports
    // the referent's own constant as non-constant, and `let r = &CONST` then
    // carries nothing for the reads through `r` to project out of.
    let Lattice::Const(v) = lattice_of(&shared_ref(
        array_literal(
            TypeTable::I32,
            vec![
                int_lit(10, TypeTable::I32, "10"),
                int_lit(20, TypeTable::I32, "20"),
            ],
        ),
        TypeTable::I32,
    )) else {
        panic!("a reference to a constant container denotes that container");
    };
    assert_eq!(
        v.field(SeqField::Len.index()).and_then(Value::as_int),
        Some((2, PrimitiveType::I32))
    );
}

#[test]
fn array_get_reads_an_element_out_of_a_constant_global() {
    // `TABLE[1]` on `global TABLE: List<i32> = [10, 31];` — the container is
    // known through the global env, so the element reads out of it.
    let func_id = next_test_func_id();
    let map = ctfe_builtin_map(func_id, CtfeBuiltin::ArrayGet);
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let backing = Value::seq(
        TypeTable::I32,
        vec![
            Value::Int {
                value: 10,
                prim: PrimitiveType::I32,
            },
            Value::Int {
                value: 31,
                prim: PrimitiveType::I32,
            },
        ],
    )
    .expect("within the sequence cap");
    let mut globals = GlobalEnv::default();
    globals.insert(
        (module.clone(), "TABLE".to_string()),
        Lattice::Const(Value::aggregate(
            TypeTable::I32,
            vec![
                (SeqField::Backing.index(), backing),
                (
                    SeqField::Len.index(),
                    Value::Int {
                        value: 2,
                        prim: PrimitiveType::I32,
                    },
                ),
            ],
        )),
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&map);
    interp.with_globals(&globals);
    let call = ctfe_builtin_call(
        func_id,
        vec![
            shared_ref(
                field_access(
                    global_get(module, "TABLE", TypeTable::I32),
                    SeqField::Backing.index(),
                    "repr",
                    TypeTable::I32,
                ),
                TypeTable::I32,
            ),
            int_lit(1, TypeTable::I32, "1"),
        ],
        TypeTable::I32,
    );
    assert_eq!(flow_fold(&mut interp, &call), i32_of(31));
}

fn continue_stmt_b() -> StmtBuild {
    Rc::new(|b| ps(b, StmtKind::Continue))
}

fn labeled_block_stmt_b(label: &str, stmts: Vec<StmtBuild>) -> StmtBuild {
    let label = label.to_string();
    Rc::new(move |b| {
        let block = block_of(b, &stmts);
        ps(
            b,
            StmtKind::LabeledBlock {
                label: label.clone(),
                block,
            },
        )
    })
}

fn break_stmt_b(label: Option<&str>, value: Option<Build>) -> StmtBuild {
    let label = label.map(str::to_string);
    Rc::new(move |b| {
        let value = value.as_ref().map(|v| v(b));
        ps(
            b,
            StmtKind::Break {
                label: label.clone(),
                value,
            },
        )
    })
}

/// `{ local = value }` as a block expression, so the assignment sits inside an
/// operand rather than at statement position.
fn block_expr_assigning_local(local_index: u32, type_id: TypeId, value: Build) -> Build {
    Rc::new(move |b| {
        let target = pe(
            b,
            ExprKind::Local {
                index: local_index,
                name: String::new(),
            },
            type_id,
        );
        let value = value(b);
        let assign = pe(b, ExprKind::Assign { target, value }, TypeTable::UNIT);
        let stmt = ps(b, StmtKind::Expr(Operand::Expr(assign)));
        let block = b.blocks.push(BlockNode {
            stmts: vec![stmt],
            span: Span::default(),
        });
        Operand::Expr(pe(b, ExprKind::Block(block), TypeTable::UNIT))
    })
}

fn global_set(name: &str, type_id: TypeId, value: Build) -> Build {
    let name = name.to_string();
    Rc::new(move |b| {
        let value = value(b);
        Operand::Expr(pe(
            b,
            ExprKind::GlobalVarSet {
                module_source: ModuleSource::default(),
                name: name.clone(),
                value,
            },
            type_id,
        ))
    })
}

fn mut_ref_of_local(index: u32, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let local = pe(
            b,
            ExprKind::Local {
                index,
                name: String::new(),
            },
            type_id,
        );
        Operand::Expr(pe(
            b,
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: Operand::Expr(local),
            },
            type_id,
        ))
    })
}

/// Build a root block from `stmts` and install it as `f`'s arena body.
fn set_arena_body(f: &mut NirFunction, stmts: Vec<StmtBuild>) {
    let mut body = Body::empty();
    let ids: Vec<StmtId> = stmts.iter().map(|s| s(&mut body)).collect();
    body.root = body.blocks.push(BlockNode {
        stmts: ids,
        span: Span::default(),
    });
    f.body = Some(body);
}

/// A process-wide counter minting a fresh [`FuncId`] per test function, so a
/// `make_pure_fn` result and the `call_expr` / `build_callee_map_test` that
/// reference it agree on the callee identity (production code stamps these in
/// `lower`).
fn next_test_func_id() -> wado_compiler::nir::FuncId {
    use cranelift_entity::EntityRef;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    wado_compiler::nir::FuncId::new(NEXT.fetch_add(1, Ordering::Relaxed) as usize)
}

fn make_pure_fn(
    name: &str,
    params: Vec<(&str, TypeId)>,
    return_type: TypeId,
    body_stmt: StmtBuild,
) -> NirFunction {
    make_pure_fn_stmts(name, params, return_type, vec![body_stmt])
}

fn make_pure_fn_stmts(
    name: &str,
    params: Vec<(&str, TypeId)>,
    return_type: TypeId,
    body_stmts: Vec<StmtBuild>,
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
            is_mut_ref: false,
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
    let mut f = NirFunction {
        id: Some(next_test_func_id()),
        is_dead: false,
        name: name.to_string(),
        module_source: ModuleSource::default(),
        visibility: wado_compiler::ast::Visibility::Public,
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
        body: None,
        span,
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
    };
    set_arena_body(&mut f, body_stmts);
    f
}

/// Build a `Call` expression targeting `func` with the given args.
/// Mirrors what the elaborator emits for a free function call.
fn call_expr(func: &NirFunction, args: Vec<Build>) -> Build {
    let func_id = func.id.expect("test function must have an id");
    let return_type = func.return_type;
    Rc::new(move |b| {
        let call_args = args
            .iter()
            .map(|e| wado_compiler::nir_arena::ArenaCallArg {
                expr: e(b),
                is_mut: false,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::Call {
                func_id,
                type_args: Vec::new(),
                args: call_args,
                has_receiver: false,
            },
            return_type,
        ))
    })
}

/// A call whose arguments carry their `is_mut` flags — the shape a `&mut`
/// argument takes at a call site.
fn call_expr_args(func: &NirFunction, args: Vec<(Build, bool)>) -> Build {
    let func_id = func.id.expect("test function must have an id");
    let return_type = func.return_type;
    Rc::new(move |b| {
        let call_args = args
            .iter()
            .map(|(e, is_mut)| wado_compiler::nir_arena::ArenaCallArg {
                expr: e(b),
                is_mut: *is_mut,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::Call {
                func_id,
                type_args: Vec::new(),
                args: call_args,
                has_receiver: false,
            },
            return_type,
        ))
    })
}

/// A method call, whose receiver is the callee's first parameter.
fn method_call_expr(func: &NirFunction, receiver: Build, args: Vec<Build>) -> Build {
    let func_id = func.id.expect("test function must have an id");
    let return_type = func.return_type;
    Rc::new(move |b| {
        let receiver = receiver(b);
        let call_args = args
            .iter()
            .map(|e| wado_compiler::nir_arena::ArenaCallArg {
                expr: e(b),
                is_mut: false,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::method_call(func_id, receiver, false, call_args),
            return_type,
        ))
    })
}

/// `func` with the listed parameters declared `&mut T`.
fn with_mut_ref_params(mut func: NirFunction, indices: &[usize]) -> NirFunction {
    for i in indices {
        func.params[*i].is_mut_ref = true;
    }
    func
}

/// Build a `CalleeMap` from the supplied functions, wrapping each in
/// `Rc<RefCell<...>>` to match the production map shape.
fn build_callee_map_test(funcs: &[NirFunction]) -> CalleeMap {
    let mut map = CalleeMap::default();
    for f in funcs {
        let key = f.id.expect("test function must have an id");
        map.insert(key, Callee::new(Rc::new(RefCell::new(f.clone()))));
    }
    map
}

#[test]
fn pure_call_const_args_folds_via_return() {
    // fn double(x: i32) -> i32 { return x * 2; }
    // double(5) → 10
    let body = return_stmt(binary(
        NirBinaryOp::Mul,
        local_expr(0, TypeTable::I32),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    ));
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&double, vec![int_lit(5, TypeTable::I32, "5")]);
    assert_eq!(
        flow_fold(&mut interp, &expr),
        Some(Value::Int {
            value: 10,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn pure_call_const_args_folds_via_tail_expr() {
    // fn add(a: i32, b: i32) -> i32 { a + b }   (expression-bodied)
    let body = expr_stmt_b(binary(
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

    let expr = call_expr(
        &add,
        vec![
            int_lit(40, TypeTable::I32, "40"),
            int_lit(2, TypeTable::I32, "2"),
        ],
    );
    assert_eq!(
        flow_fold(&mut interp, &expr),
        Some(Value::Int {
            value: 42,
            prim: PrimitiveType::I32
        })
    );
}

#[test]
fn pure_call_chained_folds_two_levels() {
    // fn double(x) { return x * 2 }
    // Bottom-up chaining: the inner call folds, then the outer call over the
    // now-constant arg folds again. Operand promotion is the engine sink's job;
    // here each level is folded directly through `flow_fold`.
    let body = return_stmt(binary(
        NirBinaryOp::Mul,
        local_expr(0, TypeTable::I32),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    ));
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let i32_of = |value: u64| {
        Some(Value::Int {
            value,
            prim: PrimitiveType::I32,
        })
    };
    // double(3) → 6.
    let inner = call_expr(&double, vec![int_lit(3, TypeTable::I32, "3")]);
    assert_eq!(flow_fold(&mut interp, &inner), i32_of(6));
    // double(6) → 12 (the outer level over the inner's folded value).
    let outer = call_expr(&double, vec![int_lit(6, TypeTable::I32, "6")]);
    assert_eq!(flow_fold(&mut interp, &outer), i32_of(12));
}

#[test]
fn pure_call_nonconst_arg_left_intact() {
    // double(x) where x has no env binding — arg is Unevaluated, so
    // the call must not be folded.
    let body = return_stmt(binary(
        NirBinaryOp::Mul,
        local_expr(0, TypeTable::I32),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    ));
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&double, vec![local_expr(7, TypeTable::I32)]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn non_pure_call_with_effect_left_intact() {
    // A function carrying any effect is not CTFE-eligible — the
    // CalleeMap excludes it, so the call stays a Call.
    let body = return_stmt(int_lit(42, TypeTable::I32, "42"));
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

    let expr = call_expr(&greet, vec![]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

/// The `Value` a folded `i32` expression yields.
fn i32_of(value: u64) -> Option<Value> {
    Some(Value::Int {
        value,
        prim: PrimitiveType::I32,
    })
}

/// Append locals after the params, which occupy `0..params.len()`.
fn push_locals(f: &mut NirFunction, locals: &[(&str, TypeId, bool)]) {
    for (name, type_id, is_mut) in locals {
        f.locals.push(NirLocal {
            name: (*name).to_string(),
            type_id: *type_id,
            is_mut: *is_mut,
        });
    }
}

/// A CTFE-eligible callee with a multi-statement arena body.
fn make_multi_stmt_fn(
    name: &str,
    params: Vec<(&str, TypeId)>,
    return_type: TypeId,
    locals: &[(&str, TypeId, bool)],
    stmts: Vec<StmtBuild>,
) -> NirFunction {
    let mut f = make_pure_fn(name, params, return_type, return_none());
    set_arena_body(&mut f, stmts);
    push_locals(&mut f, locals);
    f
}

/// Fold `f(args)` through a fresh interpreter holding just `f`.
fn fold_call_of(f: &NirFunction, args: Vec<Build>) -> Option<Value> {
    let callees = build_callee_map_test(std::slice::from_ref(f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    flow_fold(&mut interp, &call_expr(f, args))
}

/// Assert `f(args)` is left as a `Call`.
fn assert_call_intact(f: &NirFunction, args: Vec<Build>) {
    let callees = build_callee_map_test(std::slice::from_ref(f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    let (changed, body, e) = reduce_local_into(&mut interp, &call_expr(f, args));
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

/// `base + 0 + 0 + …`, `depth` additions deep. The value is `base`'s; what it
/// buys is nodes — many of them under a single statement, which is what
/// separates a per-statement charge from a per-node one.
fn deep_add_chain(base: Build, depth: usize) -> Build {
    let mut chain = base;
    for _ in 0..depth {
        chain = binary(
            NirBinaryOp::Add,
            chain,
            int_lit(0, TypeTable::I32, "0"),
            TypeTable::I32,
        );
    }
    chain
}

/// How deep a chain has to be before the copy it makes dominates the handful
/// of statements around it.
const HEAVY_CHAIN: usize = 480;

/// Fold `f()` under `budget`, with `f` the only callee.
fn fold_call_within_budget(f: &NirFunction, budget: u32) -> Option<Value> {
    let callees = build_callee_map_test(std::slice::from_ref(f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.set_step_budget(budget);
    flow_fold(&mut interp, &call_expr(f, vec![]))
}

/// `fn f() { let mut i = 0; let mut acc = 0;
///           loop { if i >= 8 { break } acc = acc + (i + 0 + …); i = i + 1 }
///           return acc }` → 0+1+…+7 = 28
fn heavy_bodied_loop_fn() -> NirFunction {
    make_multi_stmt_fn(
        "heavy_loop",
        vec![],
        TypeTable::I32,
        &[("i", TypeTable::I32, true), ("acc", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("i", 0, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            let_mut_stmt_b("acc", 1, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            loop_stmt_b(vec![
                if_stmt_b(
                    binary(
                        NirBinaryOp::GtEq,
                        local_expr(0, TypeTable::I32),
                        int_lit(8, TypeTable::I32, "8"),
                        TypeTable::BOOL,
                    ),
                    vec![break_stmt_b(None, None)],
                    vec![],
                ),
                assign_local_stmt_b(
                    1,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(1, TypeTable::I32),
                        deep_add_chain(local_expr(0, TypeTable::I32), HEAVY_CHAIN),
                        TypeTable::I32,
                    ),
                ),
                assign_local_stmt_b(
                    0,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(0, TypeTable::I32),
                        int_lit(1, TypeTable::I32, "1"),
                        TypeTable::I32,
                    ),
                ),
            ]),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    )
}

#[test]
fn a_heavy_loop_still_folds_within_the_default_budget() {
    // A loop over a big body is bounded by the budget, not refused by it.
    assert_eq!(
        fold_call_within_budget(&heavy_bodied_loop_fn(), DEFAULT_STEP_BUDGET),
        i32_of(28),
    );
}

/// `fn big() { return 1 + 0 + … }` — one statement, a whole body of nodes.
fn heavy_bodied_fn() -> NirFunction {
    make_pure_fn(
        "big",
        vec![],
        TypeTable::I32,
        return_stmt(deep_add_chain(int_lit(1, TypeTable::I32, "1"), HEAVY_CHAIN)),
    )
}

/// `fn caller() { return big() + big() + … }`, `calls` calls deep.
fn repeated_caller_fn(big: &NirFunction, calls: usize) -> NirFunction {
    let mut sum = call_expr(big, vec![]);
    for _ in 1..calls {
        sum = binary(
            NirBinaryOp::Add,
            sum,
            call_expr(big, vec![]),
            TypeTable::I32,
        );
    }
    make_pure_fn("caller", vec![], TypeTable::I32, return_stmt(sum))
}

#[test]
fn repeated_heavy_calls_still_fold_within_the_default_budget() {
    let big = heavy_bodied_fn();
    let caller = repeated_caller_fn(&big, 8);
    let callees = build_callee_map_test(&[big, caller.clone()]);
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.set_step_budget(DEFAULT_STEP_BUDGET);

    assert_eq!(
        flow_fold(&mut interp, &call_expr(&caller, vec![])),
        i32_of(8)
    );
}

#[test]
fn multi_stmt_let_sequence_folds() {
    // fn f(x) { let y = x * 2; return y; }  f(3) → 6
    let f = make_multi_stmt_fn(
        "f",
        vec![("x", TypeTable::I32)],
        TypeTable::I32,
        &[("y", TypeTable::I32, false)],
        vec![
            let_stmt_b(
                "y",
                1,
                TypeTable::I32,
                binary(
                    NirBinaryOp::Mul,
                    local_expr(0, TypeTable::I32),
                    int_lit(2, TypeTable::I32, "2"),
                    TypeTable::I32,
                ),
            ),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    );
    assert_eq!(
        fold_call_of(&f, vec![int_lit(3, TypeTable::I32, "3")]),
        i32_of(6)
    );
}

#[test]
fn multi_stmt_chained_lets_fold() {
    // fn f(x) { let a = x + 1; let b = a * 3; return b; }  f(4) → 15
    let f = make_multi_stmt_fn(
        "f",
        vec![("x", TypeTable::I32)],
        TypeTable::I32,
        &[("a", TypeTable::I32, false), ("b", TypeTable::I32, false)],
        vec![
            let_stmt_b(
                "a",
                1,
                TypeTable::I32,
                binary(
                    NirBinaryOp::Add,
                    local_expr(0, TypeTable::I32),
                    int_lit(1, TypeTable::I32, "1"),
                    TypeTable::I32,
                ),
            ),
            let_stmt_b(
                "b",
                2,
                TypeTable::I32,
                binary(
                    NirBinaryOp::Mul,
                    local_expr(1, TypeTable::I32),
                    int_lit(3, TypeTable::I32, "3"),
                    TypeTable::I32,
                ),
            ),
            return_stmt(local_expr(2, TypeTable::I32)),
        ],
    );
    assert_eq!(
        fold_call_of(&f, vec![int_lit(4, TypeTable::I32, "4")]),
        i32_of(15)
    );
}

/// `fn f(x) { if x > 0 { return 1; } return 2; }`
fn early_return_fn() -> NirFunction {
    make_multi_stmt_fn(
        "f",
        vec![("x", TypeTable::I32)],
        TypeTable::I32,
        &[],
        vec![
            if_stmt_b(
                binary(
                    NirBinaryOp::Gt,
                    local_expr(0, TypeTable::I32),
                    int_lit(0, TypeTable::I32, "0"),
                    TypeTable::BOOL,
                ),
                vec![return_stmt(int_lit(1, TypeTable::I32, "1"))],
                vec![],
            ),
            return_stmt(int_lit(2, TypeTable::I32, "2")),
        ],
    )
}

#[test]
fn multi_stmt_early_return_taken() {
    let f = early_return_fn();
    assert_eq!(
        fold_call_of(&f, vec![int_lit(5, TypeTable::I32, "5")]),
        i32_of(1)
    );
}

#[test]
fn multi_stmt_early_return_falls_through() {
    let f = early_return_fn();
    assert_eq!(
        fold_call_of(&f, vec![int_lit(0, TypeTable::I32, "0")]),
        i32_of(2)
    );
}

#[test]
fn multi_stmt_assign_rebinds_local() {
    // fn f() { let mut y = 1; y = y + 41; return y; }  → 42
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("y", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("y", 0, TypeTable::I32, int_lit(1, TypeTable::I32, "1")),
            assign_local_stmt_b(
                0,
                TypeTable::I32,
                binary(
                    NirBinaryOp::Add,
                    local_expr(0, TypeTable::I32),
                    int_lit(41, TypeTable::I32, "41"),
                    TypeTable::I32,
                ),
            ),
            return_stmt(local_expr(0, TypeTable::I32)),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(42));
}

#[test]
fn multi_stmt_trailing_expr_is_the_value() {
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("y", TypeTable::I32, false)],
        vec![
            let_stmt_b("y", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            expr_stmt_b(binary(
                NirBinaryOp::Mul,
                local_expr(0, TypeTable::I32),
                int_lit(3, TypeTable::I32, "3"),
                TypeTable::I32,
            )),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(6));
}

#[test]
fn multi_stmt_undecidable_condition_bails() {
    // The condition reads a local the engine knows nothing about.
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("flag", TypeTable::BOOL, false)],
        vec![
            if_stmt_b(
                local_expr(0, TypeTable::BOOL),
                vec![return_stmt(int_lit(1, TypeTable::I32, "1"))],
                vec![],
            ),
            return_stmt(int_lit(2, TypeTable::I32, "2")),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn loop_runs_to_its_break() {
    // fn f() { loop { break; } return 7; }
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            loop_stmt_b(vec![break_stmt_b(None, None)]),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(7));
}

/// `fn f() { let mut i = 0; let mut acc = 0;
///           loop { if i >= 3 { break; } acc = acc + i * 2; i = i + 1; }
///           return acc; }` → 0 + 2 + 4 = 6.
///
/// `i * 2` and `acc + …` differ each time round, so 6 is only reached if an
/// iteration's folds do not survive into the next one.
fn accumulating_loop_fn() -> NirFunction {
    make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("i", TypeTable::I32, true), ("acc", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("i", 0, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            let_mut_stmt_b("acc", 1, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            loop_stmt_b(vec![
                if_stmt_b(
                    binary(
                        NirBinaryOp::GtEq,
                        local_expr(0, TypeTable::I32),
                        int_lit(3, TypeTable::I32, "3"),
                        TypeTable::BOOL,
                    ),
                    vec![break_stmt_b(None, None)],
                    vec![],
                ),
                assign_local_stmt_b(
                    1,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(1, TypeTable::I32),
                        binary(
                            NirBinaryOp::Mul,
                            local_expr(0, TypeTable::I32),
                            int_lit(2, TypeTable::I32, "2"),
                            TypeTable::I32,
                        ),
                        TypeTable::I32,
                    ),
                ),
                assign_local_stmt_b(
                    0,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(0, TypeTable::I32),
                        int_lit(1, TypeTable::I32, "1"),
                        TypeTable::I32,
                    ),
                ),
            ]),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    )
}

#[test]
fn loop_accumulates_across_iterations() {
    assert_eq!(fold_call_of(&accumulating_loop_fn(), vec![]), i32_of(6));
}

#[test]
fn loop_iterations_do_not_reuse_an_earlier_fold() {
    let f = accumulating_loop_fn();
    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.set_step_budget(200);
    assert_eq!(flow_fold(&mut interp, &call_expr(&f, vec![])), i32_of(6));
}

#[test]
fn loop_iteration_does_not_keep_an_earlier_structural_rewrite() {
    // fn f() { let mut i = 0; let mut acc = 0;
    //          loop { if i >= 3 { break; }
    //                 acc = acc + (if i == 0 { 10 } else { 1 });
    //                 i = i + 1; }
    //          return acc; }  → 10 + 1 + 1 = 12
    //
    // The inner `if` is an *expression*, so a constant condition collapses it
    // to the chosen arm in the body itself. Keeping that would add 10 every
    // time round.
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("i", TypeTable::I32, true), ("acc", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("i", 0, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            let_mut_stmt_b("acc", 1, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            loop_stmt_b(vec![
                if_stmt_b(
                    binary(
                        NirBinaryOp::GtEq,
                        local_expr(0, TypeTable::I32),
                        int_lit(3, TypeTable::I32, "3"),
                        TypeTable::BOOL,
                    ),
                    vec![break_stmt_b(None, None)],
                    vec![],
                ),
                assign_local_stmt_b(
                    1,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(1, TypeTable::I32),
                        if_expr(
                            binary(
                                NirBinaryOp::Eq,
                                local_expr(0, TypeTable::I32),
                                int_lit(0, TypeTable::I32, "0"),
                                TypeTable::BOOL,
                            ),
                            block_with_tail_expr(int_lit(10, TypeTable::I32, "10")),
                            Some(block_with_tail_expr(int_lit(1, TypeTable::I32, "1"))),
                            TypeTable::I32,
                        ),
                        TypeTable::I32,
                    ),
                ),
                assign_local_stmt_b(
                    0,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(0, TypeTable::I32),
                        int_lit(1, TypeTable::I32, "1"),
                        TypeTable::I32,
                    ),
                ),
            ]),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(12));
}

#[test]
fn loop_without_an_exit_exhausts_the_budget() {
    // `loop {}` — an empty body charges no statement, so the loop must charge.
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            loop_stmt_b(vec![]),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn loop_return_leaves_the_function() {
    // fn f() { loop { return 5; } }
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![loop_stmt_b(vec![return_stmt(int_lit(
            5,
            TypeTable::I32,
            "5",
        ))])],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(5));
}

#[test]
fn loop_labeled_break_escapes_to_its_block() {
    // fn f() { L: { loop { break L; } } return 7; }
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            labeled_block_stmt_b("L", vec![loop_stmt_b(vec![break_stmt_b(Some("L"), None)])]),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(7));
}

#[test]
fn loop_continue_starts_the_next_iteration() {
    // fn f() { let mut i = 0;
    //          loop { i = i + 1; if i >= 2 { break; } continue; }
    //          return i; }  → 2
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("i", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("i", 0, TypeTable::I32, int_lit(0, TypeTable::I32, "0")),
            loop_stmt_b(vec![
                assign_local_stmt_b(
                    0,
                    TypeTable::I32,
                    binary(
                        NirBinaryOp::Add,
                        local_expr(0, TypeTable::I32),
                        int_lit(1, TypeTable::I32, "1"),
                        TypeTable::I32,
                    ),
                ),
                if_stmt_b(
                    binary(
                        NirBinaryOp::GtEq,
                        local_expr(0, TypeTable::I32),
                        int_lit(2, TypeTable::I32, "2"),
                        TypeTable::BOOL,
                    ),
                    vec![break_stmt_b(None, None)],
                    vec![],
                ),
                continue_stmt_b(),
            ]),
            return_stmt(local_expr(0, TypeTable::I32)),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(2));
}

#[test]
fn multi_stmt_labeled_block_break_is_caught() {
    // fn f() { L: { break L; } return 7; }
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            labeled_block_stmt_b("L", vec![break_stmt_b(Some("L"), None)]),
            return_stmt(int_lit(7, TypeTable::I32, "7")),
        ],
    );
    assert_eq!(fold_call_of(&f, vec![]), i32_of(7));
}

#[test]
fn multi_stmt_mut_borrowed_local_blocks_fold() {
    // fn f() { let mut y = 1; sink(&mut y); return y; }
    let sink = make_pure_fn(
        "sink",
        vec![("p", TypeTable::I32)],
        TypeTable::UNIT,
        return_none(),
    );
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("y", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("y", 0, TypeTable::I32, int_lit(1, TypeTable::I32, "1")),
            expr_stmt_b(call_expr(&sink, vec![mut_ref_of_local(0, TypeTable::I32)])),
            return_stmt(local_expr(0, TypeTable::I32)),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn multi_stmt_global_write_blocks_fold() {
    // fn bump() { COUNT = 1; return 0; }
    // A global write carries no effect, so the callee is admitted — but
    // folding the call to `0` would drop the write.
    let f = make_multi_stmt_fn(
        "bump",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            expr_stmt_b(global_set(
                "COUNT",
                TypeTable::I32,
                int_lit(1, TypeTable::I32, "1"),
            )),
            return_stmt(int_lit(0, TypeTable::I32, "0")),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn multi_stmt_discarded_unfoldable_call_blocks_fold() {
    let opaque = make_pure_fn("opaque", vec![], TypeTable::I32, return_none());
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[],
        vec![
            expr_stmt_b(call_expr(&opaque, vec![])),
            return_stmt(int_lit(3, TypeTable::I32, "3")),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn multi_stmt_assign_inside_an_operand_blocks_fold() {
    // fn f() { let mut y = 1; { y = 99 }; return y; }
    // The write sits inside an expression the executor reduces rather than runs.
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("y", TypeTable::I32, true)],
        vec![
            let_mut_stmt_b("y", 0, TypeTable::I32, int_lit(1, TypeTable::I32, "1")),
            expr_stmt_b(block_expr_assigning_local(
                0,
                TypeTable::I32,
                int_lit(99, TypeTable::I32, "99"),
            )),
            return_stmt(local_expr(0, TypeTable::I32)),
        ],
    );
    assert_call_intact(&f, vec![]);
}

#[test]
fn multi_stmt_aggregate_let_projects_a_field() {
    // fn f() { let p = Point { x: 10, y: 32 }; return p.x; }  → 10
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("p", point, false)],
        vec![
            let_stmt_b("p", 0, point, point_lit(point)),
            return_stmt(field_access(local_expr(0, point), 0, "x", TypeTable::I32)),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    assert_eq!(flow_fold(&mut interp, &call_expr(&f, vec![])), i32_of(10));
}

#[test]
fn multi_stmt_step_budget_bails() {
    let f = make_multi_stmt_fn(
        "f",
        vec![],
        TypeTable::I32,
        &[("a", TypeTable::I32, false), ("b", TypeTable::I32, false)],
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(1, TypeTable::I32, "1")),
            let_stmt_b("b", 1, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            return_stmt(local_expr(1, TypeTable::I32)),
        ],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.set_step_budget(2);
    let (changed, body, e) = reduce_local_into(&mut interp, &call_expr(&f, vec![]));
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn recursive_call_bails_via_call_stack() {
    // fn f(x) { return f(x); } — direct self-recursion. The
    // `call_stack` guard refuses re-entry on the same key, so the
    // inner `f` evaluates to Unevaluated and the outer call therefore
    // stays unfolded as well.
    let mut f = make_pure_fn(
        "f",
        vec![("x", TypeTable::I32)],
        TypeTable::I32,
        return_none(), // placeholder, replaced below
    );
    let self_call = call_expr(&f, vec![local_expr(0, TypeTable::I32)]);
    set_arena_body(&mut f, vec![return_stmt(self_call)]);

    let callees = build_callee_map_test(std::slice::from_ref(&f));
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&f, vec![int_lit(1, TypeTable::I32, "1")]);
    // Must not fold; must terminate.
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn step_budget_zero_bails() {
    // With budget set to 0 up-front, even a trivially-foldable call
    // declines. Verifies the budget gate is reached before the body
    // is touched.
    let body = return_stmt(local_expr(0, TypeTable::I32));
    let id = make_pure_fn("id", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&id));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees).set_step_budget(0);

    let expr = call_expr(&id, vec![int_lit(7, TypeTable::I32, "7")]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn body_traps_at_ctfe_left_intact() {
    // fn bad() -> i32 { return 1 / 0; }
    // The body folds to NonConst (div-by-zero), which try_call_fold
    // downgrades to Unevaluated to keep the runtime trap intact.
    let body = return_stmt(binary(
        NirBinaryOp::Div,
        int_lit(1, TypeTable::I32, "1"),
        int_lit(0, TypeTable::I32, "0"),
        TypeTable::I32,
    ));
    let bad = make_pure_fn("bad", vec![], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&bad));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&bad, vec![]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn missing_callee_left_intact() {
    // CalleeMap empty → look-up miss → no fold.
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
    let f = make_pure_fn("f", vec![], TypeTable::I32, body);
    let callees = build_callee_map_test(&[]); // empty

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&f, vec![]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn no_callee_map_means_no_fold() {
    // Without `with_callees`, every Call is Unevaluated.
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
    let f = make_pure_fn("f", vec![], TypeTable::I32, body);

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);

    let expr = call_expr(&f, vec![]);
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Call { .. }));
}

#[test]
fn ctfe_eligibility_rejects_async() {
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.is_async = true;
    assert!(!is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_accepts_inline_never() {
    // `#[inline(never)]` constrains where the body is emitted, not whether the
    // result is knowable at compile time.
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.inline_hint = InlineHint::Never;
    assert!(is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_rejects_no_body() {
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
    let mut f = make_pure_fn("f", vec![], TypeTable::I32, body);
    f.body = None;
    assert!(!is_ctfe_eligible(&f));
}

#[test]
fn ctfe_eligibility_accepts_default_pure_fn() {
    let body = return_stmt(int_lit(1, TypeTable::I32, "1"));
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
    let body = return_stmt(binary(
        NirBinaryOp::Mul,
        local_expr(0, TypeTable::I32),
        int_lit(2, TypeTable::I32, "2"),
        TypeTable::I32,
    ));
    let double = make_pure_fn("double", vec![("x", TypeTable::I32)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&double));

    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    // The bottom-up reducer folds double(5)→10 (memoizing the result), then the
    // const-true `if` collapses to the then-arm; the composed lattice is 10.
    let expr = if_expr(
        bool_lit(true),
        block_with_tail_expr(call_expr(&double, vec![int_lit(5, TypeTable::I32, "5")])),
        Some(block_with_tail_expr(int_lit(0, TypeTable::I32, "0"))),
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut interp, &expr).as_const(),
        Some(Value::Int {
            value: 10,
            prim: PrimitiveType::I32
        })
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Stage 1 (extended): GlobalEnv — `GlobalVarGet` rewriting and lattice lookup
// ──────────────────────────────────────────────────────────────────────────────

fn global_get(module: ModuleSource, name: &str, type_id: TypeId) -> Build {
    let name = name.to_string();
    Rc::new(move |b| {
        Operand::Expr(pe(
            b,
            ExprKind::GlobalVarGet {
                module_source: module.clone(),
                name: name.clone(),
            },
            type_id,
        ))
    })
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

    let expr = global_get(module, "X", TypeTable::I32);
    assert_eq!(
        flow_fold(&mut interp, &expr),
        Some(Value::Int {
            value: 42,
            prim: PrimitiveType::I32
        })
    );
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

    let expr = binary(
        NirBinaryOp::Add,
        global_get(module, "X", TypeTable::I32),
        int_lit(5, TypeTable::I32, "5"),
        TypeTable::I32,
    );
    assert_eq!(
        flow_fold(&mut interp, &expr),
        Some(Value::Int {
            value: 15,
            prim: PrimitiveType::I32
        })
    );
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

    let lat = reduce_lat(
        &mut interp,
        &global_get(module.clone(), "X", TypeTable::I32),
    );
    assert_eq!(lat, Lattice::NonConst);

    let expr = binary(
        NirBinaryOp::Add,
        global_get(module, "X", TypeTable::I32),
        int_lit(5, TypeTable::I32, "5"),
        TypeTable::I32,
    );
    let (changed, body, e) = reduce_local_into(&mut interp, &expr);
    assert!(!changed);
    assert!(matches!(body.exprs[e].kind, ExprKind::Binary { .. }));
}

#[test]
fn global_absent_stays_unevaluated() {
    // No `with_globals` installed → `GlobalVarGet` reports `Unevaluated`
    // (engine has no information). Same convention as un-bound locals.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut interp = Interpreter::new(&table);
    let lat = reduce_lat(
        &mut interp,
        &global_get(module.clone(), "MISSING", TypeTable::I32),
    );
    assert_eq!(lat, Lattice::Unevaluated);

    // With an empty `GlobalEnv` installed, an unknown key still reports
    // `Unevaluated` — no NonConst materializes spuriously.
    let globals = GlobalEnv::default();
    interp.with_globals(&globals);
    let lat = reduce_lat(&mut interp, &global_get(module, "MISSING", TypeTable::I32));
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

    let expr = global_get(module, "ENABLED", TypeTable::BOOL);
    assert_eq!(flow_fold(&mut interp, &expr), Some(Value::Bool(true)));
}

// ──────────────────────────────────────────────────────────────────────────────
// Aggregates: struct / tuple values, field projection, aggregate patterns
// ──────────────────────────────────────────────────────────────────────────────

fn point_type(table: &mut TypeTable) -> TypeId {
    table.make_struct("Point".to_string(), ModuleSource::default())
}

fn struct_lit(type_id: TypeId, fields: Vec<(u32, &'static str, Build)>) -> Build {
    Rc::new(move |b| {
        let fields = fields
            .iter()
            .map(|(field_index, name, value)| ArenaStructField {
                name: (*name).to_string(),
                value: value(b),
                field_index: *field_index,
            })
            .collect();
        Operand::Expr(pe(
            b,
            ExprKind::StructLiteral {
                struct_type: type_id,
                struct_name: "Point".to_string(),
                fields,
            },
            type_id,
        ))
    })
}

fn tuple_lit(type_id: TypeId, elements: Vec<Build>) -> Build {
    Rc::new(move |b| {
        let elements = elements.iter().map(|e| e(b)).collect();
        Operand::Expr(pe(b, ExprKind::TupleLiteral { elements }, type_id))
    })
}

fn field_access(receiver: Build, field_index: u32, name: &'static str, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let expr = receiver(b);
        Operand::Expr(pe(
            b,
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name: name.to_string(),
            },
            type_id,
        ))
    })
}

fn struct_pat(
    struct_type: TypeId,
    fields: Vec<(u32, &'static str, PatBuild)>,
    has_rest: bool,
) -> PatBuild {
    Rc::new(move |b| {
        let fields = fields
            .iter()
            .map(|(field_index, name, pattern)| ArenaStructPatternField {
                field_name: (*name).to_string(),
                field_index: *field_index,
                pattern: pattern(b),
            })
            .collect();
        pp(
            b,
            PatKind::Struct {
                struct_type,
                fields,
                has_rest,
            },
        )
    })
}

fn int(value: u64) -> Value {
    Value::Int {
        value,
        prim: PrimitiveType::I32,
    }
}

/// `Point { x: 10, y: 32 }`.
fn point_lit(point: TypeId) -> Build {
    struct_lit(
        point,
        vec![
            (0, "x", int_lit(10, TypeTable::I32, "10")),
            (1, "y", int_lit(32, TypeTable::I32, "32")),
        ],
    )
}

fn point_value(point: TypeId) -> Value {
    Value::aggregate(point, vec![(0, int(10)), (1, int(32))])
}

#[test]
fn struct_literal_reduces_to_aggregate() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &point_lit(point)),
        Lattice::Const(point_value(point)),
    );
}

#[test]
fn aggregate_equality_ignores_field_order() {
    // The literal's field order is not part of the value: `Value::aggregate`
    // canonicalizes to `field_index` order, so two spellings of one struct
    // compare equal (what the both-arms-equal `if` collapse relies on).
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let reversed = struct_lit(
        point,
        vec![
            (1, "y", int_lit(32, TypeTable::I32, "32")),
            (0, "x", int_lit(10, TypeTable::I32, "10")),
        ],
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &reversed),
        Lattice::Const(point_value(point)),
    );
}

#[test]
fn struct_literal_with_unknown_field_is_unevaluated() {
    // An unbound local field: the aggregate is not known, and claiming
    // `NonConst` would overstate what the engine learned.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = struct_lit(
        point,
        vec![
            (0, "x", int_lit(10, TypeTable::I32, "10")),
            (1, "y", local_expr(7, TypeTable::I32)),
        ],
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Unevaluated,
    );
}

#[test]
fn struct_literal_with_non_const_field_is_non_const() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let mut interp = Interpreter::new(&table);
    interp.bind_local(7, Lattice::NonConst);
    let expr = struct_lit(
        point,
        vec![
            (0, "x", int_lit(10, TypeTable::I32, "10")),
            (1, "y", local_expr(7, TypeTable::I32)),
        ],
    );
    assert_eq!(reduce_lat(&mut interp, &expr), Lattice::NonConst);
}

#[test]
fn field_access_projects_a_struct_literal() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = field_access(point_lit(point), 1, "y", TypeTable::I32);
    assert_eq!(
        flow_fold(&mut Interpreter::new(&table), &expr),
        Some(int(32)),
    );
}

#[test]
fn field_access_projects_nested_aggregates() {
    // `Line { start: Point { x: 10, y: 32 } }.start.x` → 10.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let line = table.make_struct("Line".to_string(), ModuleSource::default());
    let expr = field_access(
        field_access(
            struct_lit(line, vec![(0, "start", point_lit(point))]),
            0,
            "start",
            point,
        ),
        0,
        "x",
        TypeTable::I32,
    );
    assert_eq!(
        flow_fold(&mut Interpreter::new(&table), &expr),
        Some(int(10)),
    );
}

#[test]
fn field_access_on_non_const_receiver_is_non_const() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let mut interp = Interpreter::new(&table);
    interp.bind_local(0, Lattice::NonConst);
    let expr = field_access(local_expr(0, point), 0, "x", TypeTable::I32);
    assert_eq!(reduce_lat(&mut interp, &expr), Lattice::NonConst);
}

#[test]
fn tuple_literal_projects_by_position() {
    let mut table = TypeTable::new();
    let pair = table.make_tuple(vec![TypeTable::I32, TypeTable::I32]);
    let expr = field_access(
        tuple_lit(
            pair,
            vec![
                int_lit(10, TypeTable::I32, "10"),
                int_lit(32, TypeTable::I32, "32"),
            ],
        ),
        1,
        "1",
        TypeTable::I32,
    );
    assert_eq!(
        flow_fold(&mut Interpreter::new(&table), &expr),
        Some(int(32)),
    );
}

#[test]
fn struct_pattern_picks_the_matching_arm() {
    // match Point { x: 10, y: 32 } { { x: 10, y: 1 } => 1009, { x: 10, y: 32 } => 1013, _ => 1019 }
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm(
                struct_pat(
                    point,
                    vec![(0, "x", lit_pat_i128(10)), (1, "y", lit_pat_i128(1))],
                    false,
                ),
                int_lit(1009, TypeTable::I32, "1009"),
            ),
            arm(
                struct_pat(
                    point,
                    vec![(0, "x", lit_pat_i128(10)), (1, "y", lit_pat_i128(32))],
                    false,
                ),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn struct_pattern_rest_ignores_unlisted_fields() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm(
                struct_pat(point, vec![(0, "x", lit_pat_i128(10))], true),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn struct_pattern_with_a_binding_field_takes_the_arm() {
    // A `Binding` sub-pattern matches whatever the field holds. The arm body
    // ignores the binding, so committing the arm loses nothing.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm(
                struct_pat(
                    point,
                    vec![
                        (0, "x", lit_pat_i128(10)),
                        (1, "y", binding_pat("y", 3, TypeTable::I32)),
                    ],
                    false,
                ),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn an_arm_body_reduces_under_its_pattern_bindings() {
    // `match Point { x: 10, y: 32 } { { x: a, y: b } => a + b }` → 42: the walk
    // of the arm body sees the bindings the pattern would make at runtime.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![arm(
            struct_pat(
                point,
                vec![
                    (0, "x", binding_pat("a", 1, TypeTable::I32)),
                    (1, "y", binding_pat("b", 2, TypeTable::I32)),
                ],
                false,
            ),
            binary(
                NirBinaryOp::Add,
                local_expr(1, TypeTable::I32),
                local_expr(2, TypeTable::I32),
                TypeTable::I32,
            ),
        )],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(42)),
    );
}

#[test]
fn a_binding_the_arm_body_reads_blocks_the_splice() {
    // Splicing the arm would strip the pattern binding, leaving the body's
    // `y` read dangling. A read the backend cannot promote blocks the splice.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm(
                struct_pat(
                    point,
                    vec![
                        (0, "x", lit_pat_i128(10)),
                        (1, "y", binding_pat("y", 3, TypeTable::I32)),
                    ],
                    false,
                ),
                local_expr(3, TypeTable::I32),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&expr);
    let mut interp = Interpreter::new(&table);
    interp.reduce_to_lattice_full(&mut body, e);
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Match { .. }),
        "the match must survive: {:?}",
        body.exprs[e].kind
    );
}

/// `{ x: __lit_1, y: __lit_2 }` — the shape the elaborator lowers a struct
/// pattern with literal fields to, its literals moved into the arm guard.
fn point_binding_pat(point: TypeId) -> PatBuild {
    struct_pat(
        point,
        vec![
            (0, "x", binding_pat("__lit_1", 1, TypeTable::I32)),
            (1, "y", binding_pat("__lit_2", 2, TypeTable::I32)),
        ],
        false,
    )
}

fn eq_lit(local: u32, value: u64, repr: &'static str) -> Build {
    binary(
        NirBinaryOp::Eq,
        local_expr(local, TypeTable::I32),
        int_lit(value, TypeTable::I32, repr),
        TypeTable::BOOL,
    )
}

#[test]
fn a_guard_over_pattern_bindings_decides_the_arm() {
    // `match Point { x: 10, y: 32 } { { x: __lit_1, y: __lit_2 }
    //      && __lit_1 == 10 && __lit_2 == 32 => 1013, _ => 1019 }`.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm_with_guard(
                point_binding_pat(point),
                binary(
                    NirBinaryOp::And,
                    eq_lit(1, 10, "10"),
                    eq_lit(2, 32, "32"),
                    TypeTable::BOOL,
                ),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn a_guard_does_not_read_a_binding_index_from_outside_the_arm() {
    // Local slots are reused, so index 1 can hold an unrelated constant outside
    // the arm. The guard must be decided under the arm's own bindings: `x` is
    // 10 there, and `10 == 99` is false however index 1 reads elsewhere.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm_with_guard(
                point_binding_pat(point),
                eq_lit(1, 99, "99"),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    let mut interp = Interpreter::new(&table);
    interp.bind_local(1, Lattice::Const(int(99)));
    assert_eq!(reduce_lat(&mut interp, &expr), Lattice::Const(int(1019)));
}

#[test]
fn a_false_guard_falls_through_to_the_next_arm() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm_with_guard(
                point_binding_pat(point),
                eq_lit(1, 99, "99"),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1019)),
    );
}

#[test]
fn an_unknown_guard_leaves_the_match_alone() {
    // The guard reads a local the engine knows nothing about, so no later arm
    // can be committed either.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm_with_guard(
                point_binding_pat(point),
                binary(
                    NirBinaryOp::Eq,
                    local_expr(7, TypeTable::I32),
                    int_lit(10, TypeTable::I32, "10"),
                    TypeTable::BOOL,
                ),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&expr);
    let mut interp = Interpreter::new(&table);
    interp.reduce_to_lattice_full(&mut body, e);
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Match { .. }),
        "the match must survive: {:?}",
        body.exprs[e].kind
    );
}

#[test]
fn a_guard_over_tuple_bindings_decides_the_arm() {
    let mut table = TypeTable::new();
    let pair = table.make_tuple(vec![TypeTable::I32, TypeTable::I32]);
    let expr = match_expr(
        tuple_lit(
            pair,
            vec![
                int_lit(10, TypeTable::I32, "10"),
                int_lit(32, TypeTable::I32, "32"),
            ],
        ),
        vec![
            arm_with_guard(
                tuple_pat(
                    vec![
                        binding_pat("__lit_1", 1, TypeTable::I32),
                        binding_pat("__lit_2", 2, TypeTable::I32),
                    ],
                    false,
                ),
                eq_lit(2, 32, "32"),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn a_guard_the_engine_cannot_evaluate_blocks_a_later_arm() {
    // The first arm's guard is unknown; the wildcard arm below it must not be
    // committed, even though its pattern always matches.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm_with_guard(
                wildcard_pat(),
                local_expr(7, TypeTable::BOOL),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&expr);
    let mut interp = Interpreter::new(&table);
    interp.reduce_to_lattice_full(&mut body, e);
    assert!(
        matches!(body.exprs[e].kind, ExprKind::Match { .. }),
        "the match must survive: {:?}",
        body.exprs[e].kind
    );
}

#[test]
fn struct_pattern_rules_an_arm_out_despite_a_binding() {
    // A definite field mismatch decides the arm even when a sibling field
    // binds: dropping an arm that cannot match is always sound.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let expr = match_expr(
        point_lit(point),
        vec![
            arm(
                struct_pat(
                    point,
                    vec![
                        (0, "x", lit_pat_i128(99)),
                        (1, "y", binding_pat("y", 3, TypeTable::I32)),
                    ],
                    false,
                ),
                int_lit(1009, TypeTable::I32, "1009"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1019)),
    );
}

#[test]
fn tuple_pattern_picks_the_matching_arm() {
    let mut table = TypeTable::new();
    let pair = table.make_tuple(vec![TypeTable::I32, TypeTable::I32]);
    let expr = match_expr(
        tuple_lit(
            pair,
            vec![
                int_lit(10, TypeTable::I32, "10"),
                int_lit(32, TypeTable::I32, "32"),
            ],
        ),
        vec![
            arm(
                tuple_pat(vec![lit_pat_i128(10), lit_pat_i128(1)], false),
                int_lit(1009, TypeTable::I32, "1009"),
            ),
            arm(
                tuple_pat(vec![lit_pat_i128(10), lit_pat_i128(32)], false),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::Const(int(1013)),
    );
}

#[test]
fn tuple_pattern_with_rest_stays_unknown() {
    // `(10, ..)` leaves the trailing sub-patterns without a fixed element
    // index, so the engine does not model it.
    let mut table = TypeTable::new();
    let pair = table.make_tuple(vec![TypeTable::I32, TypeTable::I32]);
    let expr = match_expr(
        tuple_lit(
            pair,
            vec![
                int_lit(10, TypeTable::I32, "10"),
                int_lit(32, TypeTable::I32, "32"),
            ],
        ),
        vec![
            arm(
                tuple_pat(vec![lit_pat_i128(10)], true),
                int_lit(1013, TypeTable::I32, "1013"),
            ),
            arm(wildcard_pat(), int_lit(1019, TypeTable::I32, "1019")),
        ],
        TypeTable::I32,
    );
    assert_eq!(
        reduce_lat(&mut Interpreter::new(&table), &expr),
        Lattice::NonConst,
    );
}

#[test]
fn pure_call_folds_a_struct_argument() {
    // fn manhattan(p: Point) -> i32 { return p.x + p.y; }
    // manhattan(Point { x: 10, y: 32 }) → 42
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let body = return_stmt(binary(
        NirBinaryOp::Add,
        field_access(local_expr(0, point), 0, "x", TypeTable::I32),
        field_access(local_expr(0, point), 1, "y", TypeTable::I32),
        TypeTable::I32,
    ));
    let manhattan = make_pure_fn("manhattan", vec![("p", point)], TypeTable::I32, body);
    let callees = build_callee_map_test(std::slice::from_ref(&manhattan));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = call_expr(&manhattan, vec![point_lit(point)]);
    assert_eq!(flow_fold(&mut interp, &expr), Some(int(42)));
}

#[test]
fn pure_call_returning_a_struct_projects_at_the_call_site() {
    // fn origin() -> Point { return Point { x: 10, y: 32 }; }
    // origin().y → 32
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let origin = make_pure_fn("origin", vec![], point, return_stmt(point_lit(point)));
    let callees = build_callee_map_test(std::slice::from_ref(&origin));

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);

    let expr = field_access(call_expr(&origin, vec![]), 1, "y", TypeTable::I32);
    assert_eq!(flow_fold(&mut interp, &expr), Some(int(32)));
}

#[test]
fn aggregate_binding_needs_a_read_only_local() {
    // `record_aggregate_locals` decides whether a local may carry an aggregate
    // constant. Each phase below mirrors what the driving visitor does per
    // function: `enter_function`, record, then bind.
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let mut interp = Interpreter::new(&table);
    let read = field_access(local_expr(0, point), 0, "x", TypeTable::I32);

    // Without a recorded body, no local qualifies: the binding degrades to
    // `NonConst` — the engine tracks whole values, not the heap.
    interp.enter_function();
    interp.bind_local(0, Lattice::Const(point_value(point)));
    assert_eq!(reduce_lat(&mut interp, &read), Lattice::NonConst);

    // A body whose only mention of local 0 is a field read qualifies it.
    let (body, _) = into_body(&read);
    interp.enter_function();
    interp.record_aggregate_locals(&body);
    interp.bind_local(0, Lattice::Const(point_value(point)));
    assert_eq!(reduce_lat(&mut interp, &read), Lattice::Const(int(10)));

    // A body that also borrows the local mutably does not.
    let borrowed = unary(NirUnaryOp::MutRef, local_expr(0, point), point);
    let read_then_borrow: Build = Rc::new(move |b| {
        read(b);
        borrowed(b)
    });
    let (body, _) = into_body(&read_then_borrow);
    interp.enter_function();
    interp.record_aggregate_locals(&body);
    interp.bind_local(0, Lattice::Const(point_value(point)));
    let reread = field_access(local_expr(0, point), 0, "x", TypeTable::I32);
    assert_eq!(reduce_lat(&mut interp, &reread), Lattice::NonConst);
}

/// A reachable mention of local 0 whose only live parent is a position the
/// mention scan does not list (`Unary::Neg`), so any read-position witness
/// must come from elsewhere in the arena. The rule these tests pin lives on
/// `aggregate_safe_locals`.
fn body_with_unlisted_mention(point: TypeId) -> (Body, ExprId) {
    let mut body = Body::empty();
    let mention = pe(
        &mut body,
        ExprKind::Local {
            index: 0,
            name: "l0".to_string(),
        },
        point,
    );
    let unlisted_parent = pe(
        &mut body,
        ExprKind::Unary {
            op: NirUnaryOp::Neg,
            expr: Operand::Expr(mention),
        },
        point,
    );
    let live = ps(&mut body, StmtKind::Expr(Operand::Expr(unlisted_parent)));
    body.root = body.blocks.push(BlockNode {
        stmts: vec![live],
        span: Span::default(),
    });
    (body, mention)
}

#[test]
fn a_displaced_parent_still_vouches_for_its_mention() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let (mut body, mention) = body_with_unlisted_mention(point);
    // The witness: a statement no block lists, holding the same mention id.
    ps(&mut body, StmtKind::Expr(Operand::Expr(mention)));

    let mut interp = Interpreter::new(&table);
    interp.enter_function();
    interp.record_aggregate_locals(&body);
    interp.bind_local(0, Lattice::Const(point_value(point)));
    let read = field_access(local_expr(0, point), 0, "x", TypeTable::I32);
    assert_eq!(reduce_lat(&mut interp, &read), Lattice::Const(int(10)));
}

#[test]
fn a_displaced_call_still_vouches_for_its_argument() {
    let mut table = TypeTable::new();
    let point = point_type(&mut table);
    let reader = make_pure_fn(
        "reader",
        vec![("v", point)],
        TypeTable::I32,
        return_stmt(int_lit(0, TypeTable::I32, "0")),
    );
    let callees = build_callee_map_test(std::slice::from_ref(&reader));

    let (mut body, mention) = body_with_unlisted_mention(point);
    // The witness: a call expression no live node refers to, passing the same
    // mention id by value.
    pe(
        &mut body,
        ExprKind::Call {
            func_id: reader.id.expect("test function must have an id"),
            type_args: Vec::new(),
            args: vec![wado_compiler::nir_arena::ArenaCallArg {
                expr: Operand::Expr(mention),
                is_mut: false,
            }],
            has_receiver: false,
        },
        TypeTable::I32,
    );

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.enter_function();
    interp.record_aggregate_locals(&body);
    interp.bind_local(0, Lattice::Const(point_value(point)));
    let read = field_access(local_expr(0, point), 0, "x", TypeTable::I32);
    assert_eq!(reduce_lat(&mut interp, &read), Lattice::Const(int(10)));
}

#[test]
fn a_walk_that_performs_nothing_drops_a_container_a_call_writes() {
    // The exemptions belong to a compile-time frame, which performs the write
    // or abandons the evaluation. An ordinary walk performs nothing: it steps
    // over the call, so a container bound across one would answer `len()` with
    // the length the push already changed — and the bounds check folds against
    // that answer.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());

    let bump = with_mut_ref_params(
        make_pure_fn_stmts(
            "List<u8>::bump",
            vec![("self", list_ty)],
            TypeTable::UNIT,
            vec![bump_used(list_ty)],
        ),
        &[0],
    );
    let callees = build_callee_map_test(std::slice::from_ref(&bump));

    let written: Build = Rc::new(move |b| {
        used_of_local(list_ty)(b);
        method_call_expr(&bump, local_expr(0, list_ty), Vec::new())(b)
    });
    let (body, _) = into_body(&written);

    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.enter_function();
    interp.record_aggregate_locals(&body);
    interp.bind_local(
        0,
        Lattice::Const(Value::aggregate(
            list_ty,
            vec![(SeqField::Len.index(), int(2))],
        )),
    );

    assert_eq!(
        reduce_lat(&mut interp, &used_of_local(list_ty)),
        Lattice::NonConst,
    );
}

#[test]
fn aggregate_scalar_bindings_are_unaffected_by_the_read_only_rule() {
    // The aggregate gate must not touch scalars: no body scanned, yet a scalar
    // binding still folds.
    let table = TypeTable::new();
    let mut interp = Interpreter::new(&table);
    interp.bind_local(0, Lattice::Const(int(7)));
    assert_eq!(
        reduce_lat(&mut interp, &local_expr(0, TypeTable::I32)),
        Lattice::Const(int(7)),
    );
}

#[test]
fn join_keeps_signed_zeros_apart() {
    let neg = Lattice::Const(Value::Float {
        value: -0.0,
        prim: PrimitiveType::F64,
    });
    let pos = Lattice::Const(Value::Float {
        value: 0.0,
        prim: PrimitiveType::F64,
    });
    assert_eq!(neg.join(pos), Lattice::NonConst);
}

#[test]
fn if_with_signed_zero_arms_does_not_collapse() {
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(float_lit(-0.0, TypeTable::F64, "-0.0")),
        Some(block_with_tail_expr(float_lit(0.0, TypeTable::F64, "0.0"))),
        TypeTable::F64,
    );
    assert_eq!(lattice_of(&expr), Lattice::NonConst);
}

#[test]
fn if_with_identical_zero_arms_collapses() {
    let expr = if_expr(
        local_expr(0, TypeTable::BOOL),
        block_with_tail_expr(float_lit(0.0, TypeTable::F64, "0.0")),
        Some(block_with_tail_expr(float_lit(0.0, TypeTable::F64, "0.0"))),
        TypeTable::F64,
    );
    assert_eq!(
        lattice_of(&expr),
        Lattice::Const(Value::Float {
            value: 0.0,
            prim: PrimitiveType::F64,
        })
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Regions: a closed block runs as a frame started from scratch
// ──────────────────────────────────────────────────────────────────────────────

fn block_expr_of(stmts: Vec<StmtBuild>, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let block = block_of(b, &stmts);
        Operand::Expr(pe(b, ExprKind::Block(block), type_id))
    })
}

fn labeled_block_expr_of(label: &'static str, stmts: Vec<StmtBuild>, type_id: TypeId) -> Build {
    Rc::new(move |b| {
        let block = block_of(b, &stmts);
        Operand::Expr(pe(
            b,
            ExprKind::LabeledBlock {
                label: label.to_string(),
                block,
                result_type: type_id,
            },
            type_id,
        ))
    })
}

#[test]
fn a_closed_region_folds_to_the_value_it_builds() {
    // `{ let a = 5; let b = a + 1; b }` — a block that builds its value in
    // locals of its own and yields one is as self-contained as a call body.
    let table = TypeTable::new();
    let region = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(5, TypeTable::I32, "5")),
            let_stmt_b(
                "b",
                1,
                TypeTable::I32,
                binary(
                    NirBinaryOp::Add,
                    local_expr(0, TypeTable::I32),
                    int_lit(1, TypeTable::I32, "1"),
                    TypeTable::I32,
                ),
            ),
            expr_stmt_b(local_expr(1, TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let lat = Interpreter::new(&table).reduce_to_lattice_full(&mut body, e);
    assert_eq!(
        lat,
        Lattice::Const(Value::Int {
            value: 6,
            prim: PrimitiveType::I32,
        })
    );
}

#[test]
fn a_region_write_through_an_alias_lands_in_the_borrowed_local() {
    // The template shape: the buffer is written through a `let p = &mut c`
    // binding, so the write must reach `c`'s value rather than a copy bound at
    // borrow time.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let alias_backing = || {
        field_access(
            local_expr(1, list_ty),
            SeqField::Backing.index(),
            "repr",
            list_ty,
        )
    };
    let region = block_expr_of(
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            let_stmt_b("p", 1, list_ty, mut_ref(local_expr(0, list_ty), list_ty)),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(alias_backing(), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(7, TypeTable::U8, "7"),
                ],
                TypeTable::UNIT,
            )),
            expr_stmt_b(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(alias_backing(), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
        TypeTable::U8,
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let (mut body, e) = into_body_expr(&region);
    let lat = interp.reduce_to_lattice_full(&mut body, e);
    assert_eq!(
        lat,
        Lattice::Const(Value::Int {
            value: 7,
            prim: PrimitiveType::U8,
        })
    );
}

#[test]
fn a_region_writing_an_outer_local_is_refused() {
    // The assignment targets a local the block does not declare, so replacing
    // the block with its value would drop that write. The block survives as
    // written.
    let table = TypeTable::new();
    let region = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(5, TypeTable::I32, "5")),
            assign_local_stmt_b(9, TypeTable::I32, int_lit(1, TypeTable::I32, "1")),
            expr_stmt_b(local_expr(0, TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let lat = Interpreter::new(&table).reduce_to_lattice_full(&mut body, e);
    assert_eq!(lat, Lattice::Unevaluated);
    assert!(matches!(body.exprs[e].kind, ExprKind::Block(_)));
}

#[test]
fn a_labeled_region_folds_through_its_own_break() {
    let table = TypeTable::new();
    let region = labeled_block_expr_of(
        "__tmpl",
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            break_stmt_b(Some("__tmpl"), Some(local_expr(0, TypeTable::I32))),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let lat = Interpreter::new(&table).reduce_to_lattice_full(&mut body, e);
    assert_eq!(
        lat,
        Lattice::Const(Value::Int {
            value: 2,
            prim: PrimitiveType::I32,
        })
    );
}

#[test]
fn a_region_breaking_to_an_outer_label_is_refused() {
    // Control flow leaves the block, so its value cannot stand for it.
    let table = TypeTable::new();
    let region = labeled_block_expr_of(
        "__tmpl",
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            break_stmt_b(Some("outer"), Some(local_expr(0, TypeTable::I32))),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let lat = Interpreter::new(&table).reduce_to_lattice_full(&mut body, e);
    assert_eq!(lat, Lattice::Unevaluated);
    assert!(matches!(body.exprs[e].kind, ExprKind::LabeledBlock { .. }));
}

#[test]
fn a_region_writing_a_global_is_refused() {
    // A global write can never land in a region-declared local, so the scan
    // refuses before anything runs.
    let table = TypeTable::new();
    let region = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(5, TypeTable::I32, "5")),
            expr_stmt_b(global_set(
                "G",
                TypeTable::I32,
                int_lit(1, TypeTable::I32, "1"),
            )),
            expr_stmt_b(local_expr(0, TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let lat = Interpreter::new(&table).reduce_to_lattice_full(&mut body, e);
    assert_eq!(lat, Lattice::Unevaluated);
}

#[test]
fn a_region_write_behind_a_cast_still_lands() {
    // Monomorphization wraps builtin borrows in reference-shaped casts
    // (`&mut c.repr as &mut Array<u8>`). Place naming reads through them, so
    // the region still folds — refusing it would cost exactly the shape the
    // inlined stdlib append path has.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let backing = || {
        field_access(
            local_expr(0, list_ty),
            SeqField::Backing.index(),
            "repr",
            list_ty,
        )
    };
    let region = block_expr_of(
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    cast_expr(mut_ref(backing(), list_ty), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(9, TypeTable::U8, "9"),
                ],
                TypeTable::UNIT,
            )),
            expr_stmt_b(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(backing(), list_ty),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
        TypeTable::U8,
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let (mut body, e) = into_body_expr(&region);
    let lat = interp.reduce_to_lattice_full(&mut body, e);
    assert_eq!(
        lat,
        Lattice::Const(Value::Int {
            value: 9,
            prim: PrimitiveType::U8,
        })
    );
}

#[test]
fn an_alias_read_as_a_value_does_not_become_a_copy() {
    // A `&mut` is a reference: rebinding it (`let s = p`) makes `s` name the
    // same storage, so a write through `s` must reach `c`. Reading the alias
    // as a value instead would bind a copy, and the write would land in it
    // while `c` kept the constant it no longer holds.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let region = block_expr_of(
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            let_stmt_b("p", 1, list_ty, mut_ref(local_expr(0, list_ty), list_ty)),
            let_stmt_b("s", 2, list_ty, local_expr(1, list_ty)),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(
                        field_access(
                            local_expr(2, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(9, TypeTable::U8, "9"),
                ],
                TypeTable::UNIT,
            )),
            expr_stmt_b(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(
                        field_access(
                            local_expr(0, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
        TypeTable::U8,
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let (mut body, e) = into_body_expr(&region);
    let lat = interp.reduce_to_lattice_full(&mut body, e);
    assert_eq!(
        lat,
        Lattice::Const(Value::Int {
            value: 9,
            prim: PrimitiveType::U8,
        }),
        "the rebound alias must name `c`, not a copy of it",
    );
}

#[test]
fn an_alias_captured_in_an_aggregate_is_not_a_constant() {
    // A struct field holding a `&mut` — `Formatter { buf: &mut __r }` — would
    // have to carry the place, not the referent's value. The engine has no
    // such value, so capturing one is not a constant: a write through the
    // field would otherwise land in the copy while `c` kept a value it no
    // longer holds.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let holder_ty = table.make_struct("Holder".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let region = block_expr_of(
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            let_stmt_b("p", 1, list_ty, mut_ref(local_expr(0, list_ty), list_ty)),
            let_stmt_b(
                "h",
                2,
                holder_ty,
                struct_lit(holder_ty, vec![(0, "buf", local_expr(1, list_ty))]),
            ),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(
                        field_access(
                            field_access(local_expr(2, holder_ty), 0, "buf", list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(9, TypeTable::U8, "9"),
                ],
                TypeTable::UNIT,
            )),
            expr_stmt_b(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(
                        field_access(
                            local_expr(0, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
        TypeTable::U8,
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let (mut body, e) = into_body_expr(&region);
    assert_eq!(
        interp.reduce_to_lattice_full(&mut body, e),
        Lattice::Unevaluated,
        "a captured reference must not fold to the referent it copied",
    );
}

#[test]
fn a_deref_read_binds_a_copy_not_the_place() {
    // `let v = *p` reads *through* the reference, so `v` is a copy and a later
    // write through `p` must not show up in it. Deciding by place shape would
    // say otherwise: a write target wants the storage behind a deref, which is
    // why place naming peels one and this does not.
    let mut table = TypeTable::new();
    let list_ty = table.make_struct("List<u8>".to_string(), ModuleSource::default());
    let set_id = next_test_func_id();
    let get_id = next_test_func_id();
    let mut builtins = ctfe_builtin_map(set_id, CtfeBuiltin::ArraySet);
    builtins.insert(get_id, CtfeBuiltin::ArrayGet);

    let region = block_expr_of(
        vec![
            let_mut_stmt_b("c", 0, list_ty, seq_lit(list_ty, vec![0, 0])),
            let_stmt_b("p", 1, list_ty, mut_ref(local_expr(0, list_ty), list_ty)),
            let_stmt_b(
                "v",
                2,
                list_ty,
                unary(NirUnaryOp::Deref, local_expr(1, list_ty), list_ty),
            ),
            expr_stmt_b(seq_write_call(
                set_id,
                vec![
                    mut_ref(
                        field_access(
                            local_expr(1, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                    int_lit(9, TypeTable::U8, "9"),
                ],
                TypeTable::UNIT,
            )),
            expr_stmt_b(ctfe_builtin_call(
                get_id,
                vec![
                    shared_ref(
                        field_access(
                            local_expr(2, list_ty),
                            SeqField::Backing.index(),
                            "repr",
                            list_ty,
                        ),
                        list_ty,
                    ),
                    int_lit(1, TypeTable::I32, "1"),
                ],
                TypeTable::U8,
            )),
        ],
        TypeTable::U8,
    );
    let mut interp = Interpreter::new(&table);
    interp.with_ctfe_builtins(&builtins);
    let (mut body, e) = into_body_expr(&region);
    assert_eq!(
        interp.reduce_to_lattice_full(&mut body, e),
        Lattice::Const(Value::Int {
            value: 0,
            prim: PrimitiveType::U8,
        }),
        "the deref bound a copy, so the write through `p` must not reach it",
    );
}

#[test]
fn a_unit_typed_region_does_not_fold_to_its_last_value() {
    // Inlining `g(b);` leaves a block whose trailing statement still carries
    // the callee's result while the block itself stands where the program
    // expects nothing. The value must not be substituted there.
    let table = TypeTable::new();
    let region = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            expr_stmt_b(binary(
                NirBinaryOp::Add,
                local_expr(0, TypeTable::I32),
                int_lit(1, TypeTable::I32, "1"),
                TypeTable::I32,
            )),
        ],
        TypeTable::UNIT,
    );
    let (mut body, e) = into_body_expr(&region);
    let mut interp = Interpreter::new(&table);
    assert!(
        !interp.reduce_local_in_body(&mut body, e),
        "a block yielding nothing has no value to stand in for it",
    );
    assert!(matches!(body.exprs[e].kind, ExprKind::Block(_)));

    // The same region typed as what it computes still folds, so the refusal
    // above is about the unit position and not about the shape.
    let valued = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            expr_stmt_b(binary(
                NirBinaryOp::Add,
                local_expr(0, TypeTable::I32),
                int_lit(1, TypeTable::I32, "1"),
                TypeTable::I32,
            )),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&valued);
    assert_eq!(
        Interpreter::new(&table).reduce_to_lattice_full(&mut body, e),
        Lattice::Const(Value::Int {
            value: 3,
            prim: PrimitiveType::I32,
        })
    );
}

#[test]
fn a_region_already_run_is_not_run_again() {
    // The scratch sink promotes nothing, so a scalar region records its value
    // in the fold memo. Reading it back is what keeps a region inside a
    // compile-time body from being re-run — and re-charged — at every visit:
    // with a budget for one run only, the second visit still answers.
    let table = TypeTable::new();
    let region = block_expr_of(
        vec![
            let_stmt_b("a", 0, TypeTable::I32, int_lit(2, TypeTable::I32, "2")),
            expr_stmt_b(binary(
                NirBinaryOp::Add,
                local_expr(0, TypeTable::I32),
                int_lit(1, TypeTable::I32, "1"),
                TypeTable::I32,
            )),
        ],
        TypeTable::I32,
    );
    let (mut body, e) = into_body_expr(&region);
    let mut interp = Interpreter::new(&table);
    let expected = Lattice::Const(Value::Int {
        value: 3,
        prim: PrimitiveType::I32,
    });
    interp.set_step_budget(8);
    assert_eq!(interp.reduce_to_lattice_full(&mut body, e), expected);
    interp.set_step_budget(0);
    assert_eq!(
        interp.reduce_to_lattice_full(&mut body, e),
        expected,
        "the second visit must read the memo rather than pay for a re-run",
    );
}

#[test]
fn a_ref_global_alias_survives_the_body_growing_under_it() {
    // The aliases are recorded once per function, and the walk that follows
    // allocates nodes as it folds. A read through an alias is a read in the
    // same body whatever the arena has grown to since.
    let table = TypeTable::new();
    let module = ModuleSource::default();
    let mut fields = GlobalFieldEnv::default();
    fields.insert(
        (module.clone(), "CONFIG".to_string()),
        [(
            "width".to_string(),
            Value::Int {
                value: 7,
                prim: PrimitiveType::I32,
            },
        )]
        .into_iter()
        .collect(),
    );

    let mut body = Body::empty();
    let stmts = [let_stmt_b(
        "cfg",
        0,
        TypeTable::I32,
        unary(
            NirUnaryOp::Ref,
            global_get(module, "CONFIG", TypeTable::I32),
            TypeTable::I32,
        ),
    )];
    body.root = block_of(&mut body, &stmts);
    let read = field_access(local_expr(0, TypeTable::I32), 0, "width", TypeTable::I32)(&mut body)
        .as_expr()
        .expect("a field access is a composite expression");

    let mut interp = Interpreter::new(&table);
    interp.with_global_fields(&fields);
    interp.record_ref_global_aliases(&body);

    // What folding does to the body between recording an alias and reading
    // through one: a rewrite interns a node the arena did not hold before.
    local_expr(9, TypeTable::I32)(&mut body);

    assert_eq!(
        interp.reduce_to_lattice(&body, read),
        Lattice::Const(Value::Int {
            value: 7,
            prim: PrimitiveType::I32,
        }),
        "the alias must still resolve to the global it was recorded for",
    );
}

#[test]
fn a_box_shaped_ref_returning_callee_does_not_fold_through_the_lost_alias() {
    // The scenario `a_ref_returning_callee_does_not_fold_through_the_lost_alias`
    // pins, as it looks after the boxing pass: that pass redefines the `&Inner`
    // TypeId itself into `Box<Inner>`, so `RefKind::from_resolved` no longer
    // recognises the return type as a reference. The alias is just as real, and
    // `TypeTable::is_reference_shaped` is what keeps both spellings refused.
    let mut table = TypeTable::new();
    let inner_ty = table.make_struct("Inner".to_string(), ModuleSource::default());
    let pair_ty = table.make_struct("Pair".to_string(), ModuleSource::default());
    let boxed_inner = table.make_struct("Box<Inner>".to_string(), ModuleSource::default());
    table.register_box_payload(boxed_inner, inner_ty);
    let ref_pair = table.make_ref(pair_ty);

    let pick = make_pure_fn(
        "pick",
        vec![("p", ref_pair)],
        boxed_inner,
        return_stmt(unary(
            NirUnaryOp::Ref,
            field_access(local_expr(0, ref_pair), 0, "inner", inner_ty),
            boxed_inner,
        )),
    );
    let inner_lit = struct_lit(inner_ty, vec![(0, "x", int_lit(7, TypeTable::I32, "7"))]);
    let pair_lit = struct_lit(pair_ty, vec![(0, "inner", inner_lit)]);
    let scenario = make_pure_fn_stmts(
        "scenario",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("p", 0, pair_ty, pair_lit),
            let_stmt_b(
                "a",
                1,
                boxed_inner,
                call_expr(
                    &pick,
                    vec![unary(NirUnaryOp::Ref, local_expr(0, pair_ty), ref_pair)],
                ),
            ),
            assign_stmt_b(
                field_access(
                    field_access(local_expr(0, pair_ty), 0, "inner", inner_ty),
                    0,
                    "x",
                    TypeTable::I32,
                ),
                int_lit(9, TypeTable::I32, "9"),
            ),
            return_stmt(field_access(
                local_expr(1, boxed_inner),
                0,
                "x",
                TypeTable::I32,
            )),
        ],
    );

    let funcs = [pick, scenario];
    let callees = build_callee_map_test(&funcs);
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.enter_function();
    let call = call_expr(&funcs[1], vec![]);
    assert_ne!(
        reduce_lat(&mut interp, &call),
        Lattice::Const(int(7)),
        "the frame bound pick's boxed reference as a value snapshot and \
         folded through the alias",
    );
}

#[test]
fn a_ref_returning_callee_does_not_fold_through_the_lost_alias() {
    // fn pick(p: &Pair) -> &Inner { return &p.inner; }
    // fn scenario() -> i32 {
    //     let mut p = Pair { inner: Inner { x: 7 } };
    //     let a = pick(&p);
    //     p.inner.x = 9;
    //     return a.x;
    // }
    // At run time `a` aliases `p.inner`, so scenario() == 9. A frame that
    // bound the returned reference as a value snapshot would answer 7 and
    // bake the wrong constant into every caller.
    let mut table = TypeTable::new();
    let inner_ty = table.make_struct("Inner".to_string(), ModuleSource::default());
    let pair_ty = table.make_struct("Pair".to_string(), ModuleSource::default());
    let ref_inner = table.make_ref(inner_ty);
    let ref_pair = table.make_ref(pair_ty);

    let pick = make_pure_fn(
        "pick",
        vec![("p", ref_pair)],
        ref_inner,
        return_stmt(unary(
            NirUnaryOp::Ref,
            field_access(local_expr(0, ref_pair), 0, "inner", inner_ty),
            ref_inner,
        )),
    );
    let inner_lit = struct_lit(inner_ty, vec![(0, "x", int_lit(7, TypeTable::I32, "7"))]);
    let pair_lit = struct_lit(pair_ty, vec![(0, "inner", inner_lit)]);
    let scenario = make_pure_fn_stmts(
        "scenario",
        vec![],
        TypeTable::I32,
        vec![
            let_mut_stmt_b("p", 0, pair_ty, pair_lit),
            let_stmt_b(
                "a",
                1,
                ref_inner,
                call_expr(
                    &pick,
                    vec![unary(NirUnaryOp::Ref, local_expr(0, pair_ty), ref_pair)],
                ),
            ),
            assign_stmt_b(
                field_access(
                    field_access(local_expr(0, pair_ty), 0, "inner", inner_ty),
                    0,
                    "x",
                    TypeTable::I32,
                ),
                int_lit(9, TypeTable::I32, "9"),
            ),
            return_stmt(field_access(
                local_expr(1, ref_inner),
                0,
                "x",
                TypeTable::I32,
            )),
        ],
    );

    let funcs = [pick, scenario];
    let callees = build_callee_map_test(&funcs);
    let mut interp = Interpreter::new(&table);
    interp.with_callees(&callees);
    interp.enter_function();
    let call = call_expr(&funcs[1], vec![]);
    let lat = reduce_lat(&mut interp, &call);
    assert_ne!(
        lat,
        Lattice::Const(int(7)),
        "the frame bound pick's returned reference as a value snapshot and \
         folded through the alias",
    );
}
