//! [`TypeAnnotations`] — per-[`crate::ast::AstId`] type annotations and
//! dispatch decisions recorded during the body walk.
//!
//! # Membership rule
//!
//! Add a field here when it stores a fact keyed by an [`crate::ast::AstId`]
//! (or [`crate::symbol::SymbolKey`]) produced as a *decision* by the
//! body-level elaborator: the resolved type of an expression, the chosen
//! method dispatch target, the chosen coercion, the desugar kind of a
//! TIR-direct rewrite. This is what [`super::super::reify`] (Stage 5) will
//! read in lieu of re-running inference.
//!
//! Facts that derive purely from the AST (spans, position lookup) belong
//! on [`crate::ast_index::AstIndex`], not here. Decl-level facts (function
//! return types, generic-parameter tables) belong on
//! [`super::decls::ModuleDecls`].
//!
//! Stage 3 of [`wep-2026-05-26-elaborator-rearchitecture.md`] populated
//! `local_types`; Stage 4 adds `expression_types` (per-`AstId` resolved
//! type for every expression visited by the body walk),
//! `method_dispatch` (per-`MethodCallExpr` resolved target plus the
//! receiver-adjustment kind that annotate picked), `coercions`
//! (per-`AstId` coercion choice that
//! [`super::super::Elaborator::try_coerce`] applied), and `desugars`
//! (per-`AstId` rewrite tag for the TIR-direct desugar sites: `assert`,
//! `matches`, comparison chains, `for x of …`, `while`, and compound
//! assignment).

use crate::ast::{self, AstId};
use crate::hashmap::IndexMap;
use crate::symbol::SymbolKey;
use crate::tir::{FunctionRef, TypeId};

/// Method-dispatch decision recorded by the body walk for a
/// [`crate::ast::MethodCallExpr`].
///
/// `function_ref` captures the resolved target (module, mangled name,
/// monomorph info, and method-name metadata) so the future `reify` pass
/// can emit the [`crate::tir::TirExprKind::MethodCall`] without re-running
/// trait lookup, blanket-impl selection, or method-name mangling.
/// `self_kind` carries the receiver-adjustment decision (`self` / `&self`
/// / `&mut self`) so reify can drive `adjust_receiver_for_self_kind` with
/// the same kind that annotate used.
///
/// Short-circuiting paths inside
/// [`super::super::Elaborator::resolve_method_call_with`] (tuple `.len()`
/// / `.zip()`, the static-method-as-instance error) do *not* leave an
/// entry here — they rewrite the call into a non-`MethodCall` TIR shape
/// that reify recognises from the receiver type alone. The synthetic
/// for-of `.into_iter()` / `.next()` dispatches also skip recording
/// because they have no source-level `MethodCallExpr` to attach to.
///
/// `#[allow(dead_code)]` on the fields is intentional: nothing reads them
/// yet because the consumer (`reify`, Stage 5 of the WEP) has not landed.
/// The Stage 4 contract is "the data is recorded and reachable;" the read
/// path arrives with reify.
#[derive(Clone)]
pub(crate) struct MethodDispatch {
    #[allow(dead_code)]
    pub(crate) function_ref: FunctionRef,
    #[allow(dead_code)]
    pub(crate) self_kind: ast::SelfKind,
}

/// Which sub-coercion [`super::super::Elaborator::try_coerce`] applied at
/// a given expression site.
///
/// The variant is what `reify` (Stage 5) needs to pick the same lowering
/// path without re-checking expected-type compatibility; the target type
/// comes alongside on [`CoercionChoice`].
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CoercionKind {
    /// `42 → i64`, `1.0 → f32`, `0xff_ff_ff_ff → u32`, etc.
    NumericLiteral,
    /// `null → Option<T>` for some unwrapped `T`.
    NullToOption,
    /// A `String` / template literal retagged as a newtype over `String`.
    StringNewtype,
    /// A closure literal retagged as a newtype over its fn-type.
    ClosureToFnNewtype,
    /// A tuple literal lowered through `SequenceLiteralBuilder` (Array
    /// and user-defined sequence types).
    TupleToSequence,
    /// An anonymous struct literal lowered through `KeyValueLiteralBuilder`.
    StructToMap,
}

/// Coercion decision recorded for an expression that
/// [`super::super::Elaborator::try_coerce`] adapted into its expected
/// type. See [`CoercionKind`] for the variants.
#[derive(Clone)]
pub(crate) struct CoercionChoice {
    #[allow(dead_code)]
    pub(crate) kind: CoercionKind,
    #[allow(dead_code)]
    pub(crate) target_type: TypeId,
}

/// Per-`AstId` type annotations recorded by the body walk.
#[derive(Default, Clone)]
pub(crate) struct TypeAnnotations {
    /// Resolved [`TypeId`] for each local binding, keyed by the binding's
    /// defining [`SymbolKey`]. Populated alongside
    /// [`super::bindings::ModuleBindings::local_symbols`] at every
    /// `record_local_symbol` call. Consumed by LSP inlay hints via
    /// [`crate::semantics::Semantics::local_type_name`] so `let x = 1` can
    /// render the inferred `: i32` annotation without reaching into TIR.
    pub(crate) local_types: IndexMap<SymbolKey, TypeId>,
    /// Resolved [`TypeId`] for every expression visited by
    /// [`super::super::Elaborator::resolve_expr`], keyed by the
    /// expression's [`AstId`]. Populated unconditionally at the end of the
    /// resolver wrapper so every sub-expression — including operands of
    /// binary ops, call arguments, and block trailing values — leaves an
    /// entry. The future `reify` pass (Stage 5) reads this map to set
    /// `TirExpr::type_id` without re-running type inference; LSP hover
    /// may also consult it directly.
    pub(crate) expression_types: IndexMap<AstId, TypeId>,
    /// Method-dispatch decisions recorded for each AST
    /// [`crate::ast::MethodCallExpr`] visited by the body walk, keyed by
    /// the call expression's [`AstId`]. See [`MethodDispatch`] for the
    /// data shape and the recording contract.
    pub(crate) method_dispatch: IndexMap<AstId, MethodDispatch>,
    /// Coercion decisions recorded for each expression that
    /// [`super::super::Elaborator::try_coerce`] adapted into its expected
    /// type, keyed by the source-expression's [`AstId`]. Expressions that
    /// did not need coercion (the resolved type already matched the
    /// expected type, or no `expected_type` was supplied) leave no entry.
    /// See [`CoercionChoice`] / [`CoercionKind`] for the variants.
    pub(crate) coercions: IndexMap<AstId, CoercionChoice>,
    /// Desugar-kind tag for each TIR-direct rewrite site (assert,
    /// matches, comparison chain, for-of, while, compound assignment),
    /// keyed by the enclosing AST node's [`AstId`]. See [`DesugarKind`].
    pub(crate) desugars: IndexMap<AstId, DesugarKind>,
}

/// Which TIR-direct desugar path the body walk took at a source-level
/// rewrite site. The variants enumerate every surface form whose
/// lowering bypasses synthetic AST construction (see the LSP-friendly
/// compiler architecture note in
/// `wado-compiler/CLAUDE.md`); the future `reify` pass (Stage 5) reads
/// this tag to pick the same expansion without re-deciding the shape.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DesugarKind {
    /// `assert cond[, msg];` → power-assert capture + guard expansion.
    Assert,
    /// `expr matches { PATTERN }` → two-arm `match` expression.
    Matches,
    /// `a < b < c` → `(a < b) && (b < c)` with middle-term let bindings.
    ComparisonChain,
    /// `for let v of tuple { body }` → unrolled body per element.
    ForOfTuple,
    /// `for let v of variadic_tuple { body }` → deferred `VariadicForOf`
    /// TIR node consumed by monomorphization.
    ForOfVariadic,
    /// `for let v of expr { body }` → `IntoIterator` / `next()` loop.
    ForOfIterator,
    /// `while cond { body }` → `loop { if !cond { break } body }`.
    While,
    /// `while let chain { body }` → let-chain `match` with break arm.
    WhileLetChain,
    /// `x += y` (and other compound ops) → `x = x + y` style rewrite.
    CompoundAssign,
}
