//! TIR Interpreter (tiri).
//!
//! Compile-time partial evaluator for Wado TIR. The public entry point is
//! [`Interpreter::reduce`], which takes a [`TirExpr`] and returns the most
//! reduced form possible (a literal node when the expression is fully
//! known, the original tree otherwise). Constant folding is the first
//! consumer; future passes (branch pruning, constant propagation,
//! compile-time function evaluation) will reuse the same engine.
//!
//! ```text
//! Interpreter::new(type_table).reduce(&expr) -> TirExpr
//! ```
//!
//! `reduce` is **idempotent** — `reduce(reduce(e))` is structurally equal
//! to `reduce(e)` — and **monotone** — it only moves expressions toward
//! literal form, never the reverse. Literal leaves are preserved as-is so
//! the original lexical repr (e.g. `0xFF`) survives a no-op pass.
//!
//! Today the engine handles:
//!
//! - Integer arithmetic: Add, Sub, Mul, Div, Mod
//! - Integer comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Integer bitwise: `BitAnd`, `BitOr`, `BitXor`, Shl, Shr
//! - Integer unary: Neg, `BitNot`
//! - Integer types: i8, i16, i32, i64, u8, u16, u32, u64
//! - Float arithmetic: Add, Sub, Mul, Div (skipped when result is NaN)
//! - Float comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Float unary: Neg (via sign-bit flip, safe for all values including NaN)
//! - Float types: f32, f64
//! - Boolean logical: And, Or (including identity rules `false || X → X`,
//!   `true && X → X`, `X || false → X`, `X && true → X`)
//! - Boolean equality and ordering: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//!   (`false < true`)
//! - Boolean unary: Not
//! - Char comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq` (codepoint
//!   order)
//! - Casts (`expr as T`):
//!   - int ↔ int (truncation / sign- or zero-extension)
//!   - int ↔ float (signed / unsigned conversion; float → int uses
//!     Wasm `trunc_sat` semantics: NaN ↦ 0, ±∞ saturate to MIN/MAX)
//!   - f32 ↔ f64 (rounding on demote, exact on promote)
//!   - bool → int / float (true ↦ 1 / 1.0, false ↦ 0 / 0.0)
//!   - char → int (codepoint, then truncated to target width)
//!   - u8 → char (the only int → char form the resolver permits)
//! - Local variables: immutable `let` bindings whose RHS reduces to a
//!   constant flow into the env and are read back as that constant at
//!   each use site. `let mut` and post-assign locals stay `NonConst`.
//!   The driving visitor populates the env via
//!   [`Interpreter::bind_local`] / [`Interpreter::invalidate_local`].
//! - Global variables: immutable `global FOO: T = …;` declarations
//!   whose initializer reduces to a constant flow into a project-wide
//!   [`GlobalEnv`] and are read back at every `GlobalVarGet` site.
//!   Mutable globals are recorded as `NonConst` so a parent fold like
//!   `GLOBAL_MUT + 1` reports `NonConst` rather than `Unevaluated`. The
//!   driving visitor builds the env once per pass via
//!   [`Interpreter::with_globals`].
//! - `if` expressions: a constant condition collapses to the chosen
//!   arm; a non-constant condition with both arms reducing to the same
//!   lattice constant (and an effect-free condition) folds to that
//!   constant. The unreachable arm of a constant-condition `if` is
//!   treated as an SCCP infeasible edge, so a trapping branch
//!   (`else { panic(…) }`) does not contaminate the result.
//! - `if` statements: a constant condition splices the chosen branch's
//!   stmts into the parent block via
//!   [`Interpreter::reduce_local_block`].
//! - `match` expressions: a constant scrutinee collapses to the first
//!   arm whose pattern provably matches (later arms become infeasible
//!   edges); a non-constant speculatable scrutinee with every arm
//!   reducing to the same lattice constant collapses to that constant.
//!   Modelled patterns: `_`, integer / bool / char literal, integer
//!   range (signed and unsigned), or-of the above, and `ConstantValue`
//!   whose inner expression reduces to a primitive `Value`. `Binding`,
//!   `Tuple`, `Variant`, `Struct`, `Enum`, and string / null literal
//!   patterns report `Unknown` — they never wrongly commit a match and
//!   never wrongly drop a later arm.
//! - Pure-call inlining: a free `Call` whose args all reduce to
//!   constants and whose callee was admitted to the [`CalleeMap`]
//!   (pure, non-async, monomorphic — see [`is_ctfe_eligible`]) and
//!   whose body is a single `Return { Some(_) }` or `Expr(_)`
//!   evaluates the body's tail with the args bound into a fresh local
//!   environment. The `call_stack` of in-flight callees blocks
//!   recursive re-entry; a per-pass step budget caps total CTFE work;
//!   the dynamic borrow on the shared callee `RefCell` blocks the
//!   visitor's outer `borrow_mut`. `NonConst` tail results (e.g. body
//!   contains a runtime div-by-zero) are downgraded to Unevaluated so
//!   the original Call survives and the runtime trap is preserved.
//!   `MethodCall` / `IndirectCall` / `CmRawCall` and multi-stmt bodies
//!   are out of scope.
//!
//! Float arithmetic uses native Rust IEEE 754 ops (same as Wasm), following
//! cranelift's approach: fold the result, but skip if it is NaN since NaN
//! bit patterns are nondeterministic across architectures.
//!
//! Integer division/modulo by zero and signed `MIN / -1` are left
//! unfolded so the runtime trap is preserved.
//!
//! See `docs/wep-2026-04-27-tir-interpreter.md` for the planned trajectory
//! (local-variable environment, `if` / `match` reduction, bounded loop
//! unrolling, pure function inlining, and a complementary wasm-CTFE
//! backend).

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::name::ModuleSource;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirLiteralPattern, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};

/// Three-state lattice over compile-time evaluation results.
///
/// Ordering: `Unevaluated` ⊑ `Const(v)` ⊑ `NonConst`. Equivalent to the
/// classical SCCP lattice (Wegman & Zadeck, 1991): `Unevaluated` ↔
/// `Bottom`, `NonConst` ↔ `Top`. Names favour readability over the
/// academic terms — readers familiar with the abstract-interpretation
/// literature can mentally substitute Bottom/Top.
///
/// Why three states: `Option<Value>` (the previous design) collapsed
/// "I haven't computed this yet" and "I know this isn't a constant"
/// into the same `None`, which makes memoization unsound — a cached
/// `None` can't say whether a re-attempt would succeed. The lattice
/// fixes this at the type level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lattice {
    /// No information yet. Default for un-bound locals and TIR kinds
    /// the engine doesn't currently understand (e.g. a `Call` whose
    /// callee isn't pure-foldable, `Block` past a single tail
    /// expression).
    Unevaluated,
    /// Provably reduces to this value.
    Const(Value),
    /// Cannot be a reusable constant: a `let mut` binding, the result
    /// of a runtime-only operation (e.g. `x = …`, division by zero),
    /// or a fold whose operands are themselves `NonConst`.
    NonConst,
}

impl Lattice {
    /// Project to `Some(v)` only when the result is `Const`. The right
    /// shorthand for callers whose only question is "do you have a
    /// literal for me?" — the `Unevaluated` / `NonConst` distinction
    /// is collapsed into `None`. When that distinction matters
    /// (memoization, SCCP-style joins), pattern-match the variant
    /// directly instead of going through this projection.
    #[must_use]
    pub fn as_const(&self) -> Option<Value> {
        match self {
            Self::Const(v) => Some(*v),
            _ => None,
        }
    }

    /// Join two lattice values. This is the SCCP join (least upper bound)
    /// over the chain `Unevaluated ⊑ Const(v) ⊑ NonConst`:
    ///
    /// - `Unevaluated ⊔ x = x` — an `Unevaluated` arm is treated as
    ///   contributing no information (e.g. an `if true { … }` whose
    ///   `else` is unreachable / absent: the join with the executed arm
    ///   carries the executed arm's value out).
    /// - `Const(v) ⊔ Const(v) = Const(v)` — both arms agree, the
    ///   surrounding expression has that value regardless of which arm
    ///   ran. This is what lets `if cond { 5 } else { 5 }` collapse to
    ///   `5` when the condition is effect-free.
    /// - `Const(a) ⊔ Const(b) = NonConst` (when `a ≠ b`) — arms
    ///   disagree, the merged value is non-constant.
    /// - `NonConst ⊔ _ = NonConst` — top is absorbing.
    ///
    /// The operation is commutative, associative, and idempotent, as
    /// required of a lattice join. Used by [`Interpreter::reduce_local`]
    /// to merge the lattice of an `if` expression's two arms when the
    /// condition is non-constant.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unevaluated, x) | (x, Self::Unevaluated) => x,
            (Self::NonConst, _) | (_, Self::NonConst) => Self::NonConst,
            (Self::Const(a), Self::Const(b)) if a == b => Self::Const(a),
            (Self::Const(_), Self::Const(_)) => Self::NonConst,
        }
    }
}

/// A typed compile-time value produced by the interpreter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// Integer value. `prim` carries the integer type (i8..i64, u8..u64);
    /// `value` is the raw bit pattern, sign-extended for signed types.
    Int { value: u64, prim: PrimitiveType },
    /// Floating-point value. `prim` is `F32` or `F64`. For `F32`, `value`
    /// holds the f32 result widened to f64.
    Float { value: f64, prim: PrimitiveType },
    /// Boolean value.
    Bool(bool),
    /// Unicode scalar value (`char`).
    Char(char),
}

impl Value {
    /// Returns the raw integer bit pattern, or `None` if not an int.
    #[must_use]
    pub fn as_int(&self) -> Option<(u64, PrimitiveType)> {
        match self {
            Self::Int { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the raw float value and width, or `None` if not a float.
    #[must_use]
    pub fn as_float(&self) -> Option<(f64, PrimitiveType)> {
        match self {
            Self::Float { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the boolean value, or `None` if not a bool.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the char value, or `None` if not a char.
    #[must_use]
    pub fn as_char(&self) -> Option<char> {
        match self {
            Self::Char(c) => Some(*c),
            _ => None,
        }
    }

    /// Render the value as a TIR-compatible literal repr string.
    #[must_use]
    pub fn format_repr(&self) -> String {
        match self {
            Self::Int { value, prim } => format_int_repr(*value, *prim),
            Self::Float { value, .. } => format_float_repr(*value),
            Self::Bool(b) => b.to_string(),
            Self::Char(c) => format_char_repr(*c),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Callee map
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a callee in the [`CalleeMap`]. Mirrors the shape produced
/// by `FunctionRef::full_name` so the interpreter can look up a `Call`
/// node's target without re-deriving the format.
pub type CalleeKey = (ModuleSource, String);

/// Map of CTFE-eligible callees, keyed by `(module_source, full_name)`.
///
/// Values are [`Rc<RefCell<TirFunction>>`] handles aliased with
/// [`crate::flat_package::FlatPackage::functions`], not body clones, so
/// rebuilding the map every optimizer iteration costs only refcount
/// bumps. The interpreter reads each callee via
/// [`std::cell::RefCell::try_borrow`]; the failure path catches the
/// case where the visitor is currently holding `borrow_mut` on this
/// same function (i.e. self-recursive calls inside the function being
/// walked). CTFE-internal recursion across nested folds is handled
/// separately by `Interpreter::call_stack`, since `try_borrow` permits
/// concurrent immutable borrows.
///
/// The purity / CTFE-safety gate is decided once at map construction
/// time by [`is_ctfe_eligible`], and the interpreter never re-checks
/// it. Body-shape and per-call validity (arity match, all args
/// reduce, single recognized tail expression) are checked at fold
/// time, not here.
pub type CalleeMap = IndexMap<CalleeKey, Rc<RefCell<TirFunction>>>;

// ──────────────────────────────────────────────────────────────────────────────
// Global env
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a global variable in the [`GlobalEnv`]. Mirrors the
/// `(module_source, name)` shape carried by `TirExprKind::GlobalVarGet`
/// so the interpreter can look up a `GlobalVarGet` node directly.
pub type GlobalKey = (ModuleSource, String);

/// Lattice values for module-scope globals.
///
/// Populated once per pass by the driving visitor from
/// [`crate::flat_package::FlatPackage::globals`] — typically by
/// reducing each non-`mut` global's initializer through a fresh
/// [`Interpreter`] (so initializers like `1 + 2`, `i32::MAX - 1`, or
/// pure-call expressions all collapse to `Const(_)`). Mutable globals
/// are mapped to [`Lattice::NonConst`] so reads through tiri stay
/// conservative even while the global is in scope.
///
/// The map is read at every `GlobalVarGet` lookup; absent keys default
/// to [`Lattice::Unevaluated`] (the engine simply doesn't know — same
/// rule as un-bound locals).
pub type GlobalEnv = IndexMap<GlobalKey, Lattice>;

/// Default per-pass CTFE step budget. Mirrors rustc's CTFE step counter
/// shape: a hard ceiling on the number of productive call entries
/// before the engine starts bailing. Borrow-blocked re-entries (the
/// recursion guard) bail before the budget charge, so they don't
/// consume budget; the ceiling only applies to new-frame work that
/// actually runs.
pub const DEFAULT_STEP_BUDGET: u32 = 1000;

// ──────────────────────────────────────────────────────────────────────────────
// Field knowledge
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a struct-field slot tracked by [`Interpreter::field_env`].
///
/// `(local_index, field_name)` mirrors the shape produced by
/// `FieldAccess { expr: Local(idx), field_name }` so a leaf rewrite can
/// look up a recorded value with a single map probe.
pub type FieldKey = (u32, String);

/// Per-function alias / aliasing-trackability annotations consumed by
/// the interpreter's field-knowledge bookkeeping.
///
/// These three sets are computed once per function by the driving
/// visitor (typically from the function's stable
/// `address_taken_locals` / `stores_aliased_locals` plus a body walk
/// that catches transient inlined-in copies), then handed to the
/// interpreter via [`Interpreter::set_alias_info`].
///
/// - `aliased`: locals reachable through some other handle (`&x`,
///   `&mut x`, captured by a closure, struct-field-stored, etc.).
///   Field knowledge IS recorded for these locals; the flow-sensitive
///   walk drops their entries at every side-effect boundary (call,
///   dereferenced write, …) where an unseen alias could have mutated
///   the storage.
/// - `untrackable`: locals whose aliasing escapes our analysis (e.g.
///   stashed across a `stores`-annotated callee). Field knowledge is
///   **never** recorded for these; that matches the conservatism the
///   OLD WIR-level `const_forward` had for stores-passed args.
/// - `alias_groups`: union-find groups of locals connected by
///   reference-typed `let dst = src` copies (`Box<T>`, `Array<T>`,
///   `&T`, `&mut T`). Used to widen field-assignment invalidation:
///   writing `dst.field = …` must drop the same field on every
///   alias.
#[derive(Default, Clone, Debug)]
pub struct AliasInfo {
    pub aliased: IndexSet<u32>,
    pub untrackable: IndexSet<u32>,
    pub alias_groups: IndexMap<u32, IndexSet<u32>>,
}

/// Snapshot of [`Interpreter::field_env`] returned by
/// [`Interpreter::snapshot_fields`]. Restored verbatim by
/// [`Interpreter::restore_fields`]; used by the driving visitor to
/// fork field knowledge at branch boundaries (`if`, `match`, `if let`)
/// so each arm walks against the entry state.
#[derive(Clone, Debug)]
pub struct FieldSnapshot {
    fields: IndexMap<FieldKey, Value>,
}

/// Decide whether a function may be evaluated at compile time.
///
/// The check is a conservative pure-and-safe gate, applied once when the
/// driving visitor builds the [`CalleeMap`]:
///
/// - `effects.is_empty()` — no `with` clauses (the effect system's purity
///   witness, modulo trap effects which Wado tracks separately).
/// - `body.is_some()` — has a Wado-source body. External / CM-import
///   functions have no inspectable body.
/// - `!is_cm_binding && !is_dispatch_wrapper && !is_cm_export` —
///   synthesized ABI bridges aren't real Wado functions.
/// - `!is_async && task_return_type.is_none()` — async functions
///   participate in the CM async runtime; not CTFE-safe.
/// - `stores.is_empty()` — `stores[...]` is moot for CTFE (we don't pass
///   refs), but bail conservatively.
/// - `inline_hint != InlineHint::Never` — respect the user's explicit
///   "do not expand this" annotation.
/// - `type_params` and `impl_type_params` empty — CTFE runs after
///   monomorphization, so concrete bodies only.
#[must_use]
pub fn is_ctfe_eligible(func: &TirFunction) -> bool {
    func.effects.is_empty()
        && func.body.is_some()
        && !func.is_cm_binding
        && !func.is_dispatch_wrapper
        && !func.is_cm_export
        && !func.is_async
        && func.task_return_type.is_none()
        && func.stores.is_empty()
        && func.inline_hint != crate::tir::InlineHint::Never
        && func.type_params.is_empty()
        && func.impl_type_params.is_empty()
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over [`TirExpr`].
///
/// Holds the type table needed to resolve operand widths, a per-function
/// `env` mapping local indices to lattice values, an optional
/// [`CalleeMap`] of pure-eligible callees, a step budget, and a
/// `call_stack` of in-flight CTFE frames for recursion detection.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    /// Lattice values for `let`-bound locals in the *current function*.
    /// Populated by the driving visitor via [`bind_local`] /
    /// [`invalidate_local`]; cleared via [`enter_function`]. Reads of
    /// `TirExprKind::Local` consult this map during folding.
    ///
    /// Locals not present in the map default to [`Lattice::Unevaluated`].
    ///
    /// [`bind_local`]: Self::bind_local
    /// [`invalidate_local`]: Self::invalidate_local
    /// [`enter_function`]: Self::enter_function
    env: IndexMap<u32, Lattice>,
    /// Per-(local, field) constant values for the *current function*.
    ///
    /// Populated by the driving visitor when it sees a `let local =
    /// StructLiteral { f: lit, … }`, a `local.field = lit` assignment,
    /// or a recognized `$value_copy$T(src)` / Local→Local copy that
    /// transfers field knowledge. Reads at `FieldAccess(Local(idx),
    /// field_name)` sites consult this map and rewrite the read to the
    /// recorded literal.
    ///
    /// Only the four primitive literal kinds (Int / Float / Bool /
    /// Char) — exactly the values [`Value`] models — are forwardable;
    /// `String` / `null` / aggregate fields stay un-recorded so their
    /// reads always go through the runtime.
    field_env: IndexMap<FieldKey, Value>,
    /// Per-function alias annotations driving [`field_env`]
    /// invalidation. Empty by default; populated once per function by
    /// the driving visitor via [`set_alias_info`].
    ///
    /// [`field_env`]: Self::field_env
    /// [`set_alias_info`]: Self::set_alias_info
    alias_info: AliasInfo,
    /// Pre-built map of CTFE-eligible callees. When `None`, `Call` nodes
    /// stay [`Lattice::Unevaluated`]. The visitor populates this once
    /// per pass via [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    callees: Option<&'a CalleeMap>,
    /// Pre-built lattice values for module-scope globals. When `None`,
    /// every `GlobalVarGet` stays [`Lattice::Unevaluated`]. The visitor
    /// populates this once per pass via [`with_globals`].
    ///
    /// [`with_globals`]: Self::with_globals
    globals: Option<&'a GlobalEnv>,
    /// Hard ceiling on the number of productive CTFE call entries
    /// before bailing. Decremented once per successful body evaluation;
    /// on zero, further attempts return `Unevaluated`.
    step_budget: u32,
    /// Keys of the callees whose bodies the engine is currently
    /// evaluating, in entry order. A `Call` to a key already on the
    /// stack reports `Unevaluated` immediately, so direct (`f → f`)
    /// and indirect (`f → g → f`) recursion terminate without
    /// consuming budget. The `RefCell` borrow guard cannot serve this
    /// role on its own because `try_borrow` permits concurrent
    /// immutable borrows.
    call_stack: Vec<CalleeKey>,
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            env: IndexMap::default(),
            field_env: IndexMap::default(),
            alias_info: AliasInfo::default(),
            callees: None,
            globals: None,
            step_budget: DEFAULT_STEP_BUDGET,
            call_stack: Vec::new(),
        }
    }

    /// Install the [`CalleeMap`]. Without this, every `Call` node
    /// remains [`Lattice::Unevaluated`] — the engine has no body to
    /// look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass).
    pub fn with_callees(&mut self, callees: &'a CalleeMap) -> &mut Self {
        self.callees = Some(callees);
        self
    }

    /// Install the [`GlobalEnv`]. Without this, every `GlobalVarGet`
    /// node remains [`Lattice::Unevaluated`] — the engine has no
    /// initializer lattice to look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass), mirroring [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    pub fn with_globals(&mut self, globals: &'a GlobalEnv) -> &mut Self {
        self.globals = Some(globals);
        self
    }

    /// Override the per-pass CTFE step budget (default
    /// [`DEFAULT_STEP_BUDGET`]). Called rarely — primarily by tests
    /// exercising the budget-exhaustion path.
    pub fn set_step_budget(&mut self, budget: u32) -> &mut Self {
        self.step_budget = budget;
        self
    }

    /// Reset the per-function environment. The driving visitor must call
    /// this before walking each function body; otherwise a previous
    /// function's bindings would leak into the next one (local indices
    /// are unique per function, not project-wide).
    ///
    /// Asserts the recursion guard is clear — a leaked entry would mean
    /// a previous walk panicked mid-call. The step budget is
    /// intentionally *not* touched: it caps total CTFE work across the
    /// pass, not per-function.
    pub fn enter_function(&mut self) {
        self.env.clear();
        self.field_env.clear();
        self.alias_info = AliasInfo::default();
        debug_assert!(
            self.call_stack.is_empty(),
            "tiri call_stack leaked across function boundary",
        );
    }

    /// Install per-function alias annotations. The driving visitor
    /// calls this after [`enter_function`] and before walking the
    /// body. See [`AliasInfo`] for the meaning of each set.
    ///
    /// [`enter_function`]: Self::enter_function
    pub fn set_alias_info(&mut self, info: AliasInfo) {
        self.alias_info = info;
    }

    /// Record a lattice value for a `let`-bound local. The driving
    /// visitor calls this after walking a `Let` statement: pass
    /// [`Lattice::Const`] for an immutable binding whose RHS reduced,
    /// [`Lattice::NonConst`] for `let mut` or any RHS that could not be
    /// reduced.
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
        self.env.insert(index, lattice);
    }

    /// Mark a local as definitely non-constant from this point on. The
    /// driving visitor calls this when it sees an `x = expr` assignment.
    /// Conservative — we don't track flow-sensitive new values, just
    /// invalidate the prior binding. Also drops every recorded field
    /// of the local — the new value might not have those fields, or
    /// might have different ones.
    pub fn invalidate_local(&mut self, index: u32) {
        self.env.insert(index, Lattice::NonConst);
        self.field_env.retain(|(idx, _), _| *idx != index);
    }

    /// Record `value` as the known compile-time value of
    /// `local_index.field_name`. The driving visitor calls this when
    /// it sees a `let local = StructLiteral { field_name: lit, … }`,
    /// a direct `local.field_name = lit` assignment, or a recognized
    /// field-knowledge transfer (`$value_copy$T(src)` /
    /// reference-typed `let dst = src`). Reads at `FieldAccess(Local,
    /// field_name)` consult the recorded value through
    /// [`expr_to_lattice`] / [`reduce_local`].
    ///
    /// Skipped silently when `local_index` is in the `untrackable`
    /// set — those locals participate in aliasing the optimizer can
    /// no longer see, so any later read may witness a mutation we
    /// never recorded. (Same conservatism as the WIR-level
    /// `const_forward` had for stores-passed args.)
    ///
    /// [`expr_to_lattice`]: Self::expr_to_lattice
    /// [`reduce_local`]: Self::reduce_local
    pub fn bind_field(&mut self, local_index: u32, field_name: &str, value: Value) {
        if self.alias_info.untrackable.contains(&local_index) {
            return;
        }
        self.field_env
            .insert((local_index, field_name.to_string()), value);
    }

    /// Drop the recorded value (if any) for `local_index.field_name`.
    /// The driving visitor calls this on `local.field = expr`
    /// assignments before optionally re-recording with [`bind_field`]
    /// when `expr` is a forwardable literal.
    ///
    /// Aliased locals in the same `alias_groups` entry are
    /// invalidated for the same field, since they share the
    /// underlying object's storage.
    ///
    /// [`bind_field`]: Self::bind_field
    pub fn invalidate_field(&mut self, local_index: u32, field_name: &str) {
        self.field_env
            .swap_remove(&(local_index, field_name.to_string()));
        if let Some(group) = self.alias_info.alias_groups.get(&local_index).cloned() {
            for other in &group {
                if *other == local_index {
                    continue;
                }
                self.field_env
                    .swap_remove(&(*other, field_name.to_string()));
            }
        }
    }

    /// Drop every field entry whose owning local is in
    /// `alias_info.aliased`. The driving visitor calls this at
    /// side-effect boundaries (calls, dereferenced writes) where some
    /// external code could have mutated the storage through an alias.
    pub fn invalidate_aliased_fields(&mut self) {
        if self.alias_info.aliased.is_empty() {
            return;
        }
        let aliased = &self.alias_info.aliased;
        self.field_env.retain(|(idx, _), _| !aliased.contains(idx));
    }

    /// Copy every recorded field of `src` to `dst`. Used by the
    /// driving visitor to thread field knowledge through `let dst =
    /// src` (reference-typed Local→Local copy, where both names alias
    /// the same heap object) and `let dst = $value_copy$T(src)` (a
    /// fresh deep copy that carries the same field values). Skipped
    /// when `dst` is `untrackable`.
    pub fn copy_fields_from(&mut self, src: u32, dst: u32) {
        if self.alias_info.untrackable.contains(&dst) {
            return;
        }
        let copies: Vec<(String, Value)> = self
            .field_env
            .iter()
            .filter_map(|((idx, name), v)| {
                if *idx == src {
                    Some((name.clone(), *v))
                } else {
                    None
                }
            })
            .collect();
        for (name, v) in copies {
            self.field_env.insert((dst, name), v);
        }
    }

    /// Take a snapshot of the current field environment. Used by the
    /// driving visitor to fork at branch boundaries: snapshot, walk
    /// one arm, restore, walk the other. Locals don't need this fork
    /// (the only mutation channel is `let mut`, recorded preemptively
    /// as `NonConst`); fields do, because `local.field = …` inside a
    /// branch is conditional on the branch firing.
    #[must_use]
    pub fn snapshot_fields(&self) -> FieldSnapshot {
        FieldSnapshot {
            fields: self.field_env.clone(),
        }
    }

    /// Restore a [`FieldSnapshot`] taken via [`snapshot_fields`].
    ///
    /// [`snapshot_fields`]: Self::snapshot_fields
    pub fn restore_fields(&mut self, snap: FieldSnapshot) {
        self.field_env = snap.fields;
    }

    /// Drop every recorded field. Used at control-flow merges where
    /// conservatively forgetting all fields is simpler than computing
    /// the meet of per-branch knowledge.
    pub fn clear_fields(&mut self) {
        self.field_env.clear();
    }

    /// Reduce `expr` as far as possible.
    ///
    /// Always returns a (possibly structurally-identical) [`TirExpr`].
    /// Literal leaves are preserved verbatim so their lexical repr
    /// (e.g. `0xFF`) survives a no-op pass.
    pub fn reduce(&mut self, expr: &TirExpr) -> TirExpr {
        let mut owned = expr.clone();
        self.reduce_in_place(&mut owned);
        owned
    }

    /// Recursively reduce `expr` in place over the subtree the engine
    /// currently understands (Binary / Unary / Cast / If). Returns
    /// `true` when anything changed.
    ///
    /// Internal: the only public entry points are [`reduce`] and
    /// [`reduce_local`]. `reduce` clones into `reduce_in_place`; visitor
    /// drivers that already walk every TIR kind via
    /// `tir_visitor::opt_walk_expr` should call `reduce_local` directly.
    ///
    /// [`reduce`]: Self::reduce
    /// [`reduce_local`]: Self::reduce_local
    fn reduce_in_place(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: recurse into children first so the local rewrite
        // step at this node sees fully-reduced operands.
        let mut changed = match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                let l = self.reduce_in_place(left);
                let r = self.reduce_in_place(right);
                l || r
            }
            TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
                self.reduce_in_place(inner)
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut c = self.reduce_in_place(condition);
                c |= self.reduce_in_place_block(then_branch);
                if let Some(eb) = else_branch {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let mut c = self.reduce_in_place(scrutinee);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        c |= self.reduce_in_place(g);
                    }
                    c |= self.reduce_in_place(&mut arm.body);
                }
                c
            }
            _ => false,
        };

        changed |= self.reduce_local(expr);
        changed
    }

    /// Recursively reduce every expression inside `block` in place.
    /// Used by [`reduce_in_place`] to walk into `if` arms when the
    /// engine is invoked through the owning [`reduce`] entry point
    /// (the visitor-driven path walks blocks itself).
    fn reduce_in_place_block(&mut self, block: &mut TirBlock) -> bool {
        let mut changed = false;
        for stmt in &mut block.stmts {
            changed |= self.reduce_in_place_stmt(stmt);
        }
        changed |= self.reduce_local_block(block);
        changed
    }

    fn reduce_in_place_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        match &mut stmt.kind {
            TirStmtKind::Expr(e) => self.reduce_in_place(e),
            TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
                self.reduce_in_place(value)
            }
            TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
                value.as_mut().is_some_and(|v| self.reduce_in_place(v))
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut c = self.reduce_in_place(condition);
                c |= self.reduce_in_place_block(then_block);
                if let Some(eb) = else_block {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            TirStmtKind::Loop { body } => self.reduce_in_place_block(body),
            TirStmtKind::LabeledBlock { block, .. } => self.reduce_in_place_block(block),
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                let mut c = self.reduce_in_place(scrutinee);
                c |= self.reduce_in_place_block(then_block);
                if let Some(eb) = else_block {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            TirStmtKind::Continue
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => false,
        }
    }

    /// Apply the engine's rewrite rules to `expr` only — without recursing
    /// into children. Returns `true` when `expr` was rewritten.
    ///
    /// This is the right entry point when the caller is already driving a
    /// TIR walk (for example via `tir_visitor::opt_walk_expr`) and wants
    /// to slot tiri's local rewrites into each visited node. The rules
    /// are constant folding for Binary / Unary / Cast, short-circuit
    /// identity simplifications for `&&` / `||`, pure-call inlining,
    /// constant-condition or both-arms-equal `if` collapse, and the
    /// matching `match`-expression collapse.
    ///
    /// `Local` nodes themselves are never rewritten in place: their env
    /// values are read transparently when computing the parent
    /// expression's fold (`x + 1` → fold by reading `x` from env, no
    /// in-place mutation of the `Local` node). This keeps assignment
    /// targets (`x = …`, `obj.f = …`, `arr[i] = …`) safely opaque.
    /// `GlobalVarGet` and `FieldAccess(Local, _)` are the exceptions:
    /// the dedicated leaf-rewrite arms below replace the read with the
    /// recorded `Const(v)` literal when one is available. The driving
    /// visitor must avoid calling `reduce_local` on the lvalue side of
    /// an `Assign` (i.e. on the OUTER `FieldAccess` / `Index` node of
    /// `target`) — only its sub-expressions are read positions. See
    /// `optimize::const_folding::ConstFoldVisitor::visit_expr` for the
    /// concrete guard.
    pub fn reduce_local(&mut self, expr: &mut TirExpr) -> bool {
        if let Lattice::Const(v) = self.try_fold(expr) {
            expr.kind = value_to_expr_kind(v);
            return true;
        }
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &expr.kind
            && let Lattice::Const(v) = self.global_lattice(module_source, name)
        {
            expr.kind = value_to_expr_kind(v);
            return true;
        }
        if let TirExprKind::FieldAccess {
            expr: inner,
            field_name,
            ..
        } = &expr.kind
            && let TirExprKind::Local { index, .. } = &inner.kind
            && let Some(v) = self.field_env.get(&(*index, field_name.clone())).copied()
        {
            expr.kind = value_to_expr_kind(v);
            return true;
        }
        // try_call_fold returns Const only when the whole call collapses
        // to a literal; Unevaluated / NonConst leave the Call intact so
        // any runtime trap inside the body survives.
        if let Lattice::Const(v) = self.try_call_fold(expr) {
            expr.kind = value_to_expr_kind(v);
            return true;
        }
        if rewrite_short_circuit(expr) {
            return true;
        }
        if self.rewrite_if_expr(expr) {
            return true;
        }
        self.rewrite_match_expr(expr)
    }

    /// Apply stmt-level rewrites that may expand or contract the stmt
    /// list of `block`. Currently the only such rewrite is constant-
    /// condition `if`-statement folding: an `if true { … } else { … }`
    /// stmt is replaced by the chosen branch's stmts in the parent
    /// block; an `if false { … }` with no else is dropped entirely.
    ///
    /// Returns `true` when the block was rewritten. The caller (driving
    /// visitor) is expected to have walked into each stmt's children
    /// before calling this so the conditions are already folded.
    pub fn reduce_local_block(&mut self, block: &mut TirBlock) -> bool {
        let has_constant_if = block.stmts.iter().any(|s| {
            matches!(
                &s.kind,
                TirStmtKind::If { condition, .. }
                    if matches!(condition.kind, TirExprKind::BoolLiteral(_))
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = std::mem::take(&mut block.stmts);
        for stmt in old_stmts {
            if let TirStmtKind::If { ref condition, .. } = stmt.kind
                && let TirExprKind::BoolLiteral(value) = condition.kind
            {
                let TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } = stmt.kind
                else {
                    unreachable!();
                };
                if value {
                    block.stmts.extend(then_block.stmts);
                } else if let Some(eb) = else_block {
                    block.stmts.extend(eb.stmts);
                }
                continue;
            }
            block.stmts.push(stmt);
        }
        true
    }

    /// Rewrite an `if` expression. A constant-bool condition collapses
    /// the node to the chosen arm's block; a non-constant but
    /// speculatable condition with both arms reducing to the same
    /// `Const(v)` collapses to that literal.
    fn rewrite_if_expr(&mut self, expr: &mut TirExpr) -> bool {
        let TirExprKind::If {
            condition,
            then_branch: _,
            else_branch: _,
        } = &expr.kind
        else {
            return false;
        };
        let cond_lat = self.expr_to_lattice(condition);

        // Constant condition → splice the chosen arm. The unreachable arm
        // is dropped without ever being asked for a lattice value, so a
        // trapping `else { panic(…) }` does not contaminate the result —
        // this is the SCCP "infeasible edge" treatment.
        if let Lattice::Const(Value::Bool(b)) = cond_lat {
            let TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
            else {
                unreachable!();
            };
            if b {
                expr.kind = TirExprKind::Block(then_branch);
            } else if let Some(eb) = else_branch {
                expr.kind = TirExprKind::Block(eb);
            }
            // false without else: TirExprKind::Unit is already in place.
            return true;
        }

        // Non-constant condition: consider the both-arms-equal collapse.
        // This is safe only when the condition is effect-free, since
        // dropping the `if` drops its evaluation. See
        // [`is_speculatable`] for what counts as effect-free.
        //
        // Require *both* arms to reduce to the same `Const(v)`. Using
        // `Lattice::join` here would be tempting but unsound: join's
        // `Unevaluated ⊔ Const(v) → Const(v)` rule encodes an SCCP
        // infeasible-edge semantic that does not apply when both edges
        // are feasible (an `Unevaluated` arm here means "reachable but
        // value not known", not "unreachable"). Match `Const(_)` on
        // both sides explicitly so an arm we couldn't analyze never
        // erases the surrounding `if`.
        let TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &expr.kind
        else {
            unreachable!();
        };
        if !is_speculatable(condition) {
            return false;
        }
        let Lattice::Const(t) = self.block_lattice(then_branch) else {
            return false;
        };
        let Some(eb) = else_branch else {
            return false;
        };
        let Lattice::Const(e) = self.block_lattice(eb) else {
            return false;
        };
        if t != e {
            return false;
        }
        expr.kind = value_to_expr_kind(t);
        true
    }

    /// Rewrite a `match` expression. Two reductions:
    ///
    /// 1. **Const scrutinee**: pick the first arm whose pattern provably
    ///    matches (no guard, definite `Yes`) and replace the `Match`
    ///    with `Block { stmts: [Expr(arm.body)] }`. An earlier `Unknown`
    ///    arm prevents us from proving a definite arm fires first; bail.
    /// 2. **Non-const speculatable scrutinee, all-arms-equal collapse**:
    ///    when every arm has no guard and reduces to the same
    ///    `Const(v)`, rewrite the whole match to that literal. The
    ///    same `is_speculatable` gate as the `if` rule applies, since
    ///    we're dropping the scrutinee's evaluation.
    fn rewrite_match_expr(&mut self, expr: &mut TirExpr) -> bool {
        let TirExprKind::Match {
            expr: scrutinee,
            arms,
        } = &expr.kind
        else {
            return false;
        };
        if arms.is_empty() {
            return false;
        }

        // Rule 1: const scrutinee → splice the chosen arm.
        if let Lattice::Const(scrut_v) = self.expr_to_lattice(scrutinee) {
            // Walk arms; bail at first Unknown without committing.
            let mut chosen: Option<usize> = None;
            for (i, arm) in arms.iter().enumerate() {
                if arm.guard.is_some() {
                    return false;
                }
                match self.pattern_matches(&scrut_v, &arm.pattern) {
                    PatternMatch::Yes => {
                        chosen = Some(i);
                        break;
                    }
                    PatternMatch::No => {}
                    PatternMatch::Unknown => return false,
                }
            }
            let Some(idx) = chosen else {
                return false;
            };
            let TirExprKind::Match { arms, .. } =
                std::mem::replace(&mut expr.kind, TirExprKind::Unit)
            else {
                unreachable!();
            };
            let body = arms
                .into_iter()
                .nth(idx)
                .expect("chosen index in range")
                .body;
            let span = body.span;
            expr.kind = TirExprKind::Block(TirBlock::new(
                vec![TirStmt::new(TirStmtKind::Expr(body), span)],
                span,
            ));
            return true;
        }

        // Rule 2: non-const speculatable scrutinee, all-arms-equal.
        if !is_speculatable(scrutinee) {
            return false;
        }
        if arms.iter().any(|a| a.guard.is_some()) {
            return false;
        }
        // The match must be provably exhaustive — otherwise an unmatched
        // scrutinee value would trap (the lowering inserts an
        // Unreachable fallback), and rewriting the whole expression to
        // a literal would silently drop that trap. Wado's resolver
        // skips exhaustiveness checks for some scrutinee types
        // (struct, string, tuple, …); without an unguarded catch-all
        // we cannot prove the fallback is unreachable.
        if !is_provably_exhaustive(arms) {
            return false;
        }
        let mut common: Option<Value> = None;
        for arm in arms {
            let Lattice::Const(v) = self.expr_to_lattice(&arm.body) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(c) if c != v => return false,
                Some(_) => {}
            }
        }
        let v = common.expect("at least one arm");
        expr.kind = value_to_expr_kind(v);
        true
    }

    /// Reduce `expr` to a [`Lattice`] value without mutating the
    /// caller's tree.
    ///
    /// This is the engine's canonical query API. Returns:
    ///
    /// - [`Lattice::Const(v)`] when `expr` evaluates to a known value
    ///   (literal leaf, fully-reduced Binary/Unary/Cast, or a `Local`
    ///   bound to `Const(_)` in env)
    /// - [`Lattice::NonConst`] when `expr` is a `Local` known to be
    ///   non-constant, has a `NonConst` operand, or is a fold that
    ///   meets `Const` operands but evidently fails (e.g. div-by-zero,
    ///   NaN-producing float op, `i32::MIN / -1`)
    /// - [`Lattice::Unevaluated`] when the engine can't yet decide
    ///   (un-bound `Local`, unsupported kind such as `Call` or `Block`)
    pub fn reduce_to_lattice(&mut self, expr: &TirExpr) -> Lattice {
        // First reduce children in place — a Const-Const fold inside a
        // child is observable as a literal at the parent. This may turn
        // a Binary into a literal (Const) or leave it as Binary if the
        // fold failed.
        let mut owned = expr.clone();
        self.reduce_in_place(&mut owned);
        // Compute the lattice of the (possibly partially-reduced)
        // expression. `try_fold` handles Binary / Unary / Cast directly,
        // so a Const/Const op whose runtime would trap reports
        // `NonConst` rather than collapsing to `Unevaluated` because the
        // node is structurally still a Binary. For every other kind
        // `try_fold` returns `Unevaluated`; fall through to the literal
        // / Local-env lookup.
        match self.try_fold(&owned) {
            Lattice::Unevaluated => self.expr_to_lattice(&owned),
            other => other,
        }
    }

    /// Map a (possibly already-reduced) `TirExpr` to a `Lattice`. For
    /// literal leaves this is straightforward; for a `Local` node the
    /// env is consulted; for an `if` node the SCCP-style join over the
    /// arm lattices is taken (with the unreachable arm of a
    /// constant-condition `if` excluded — `feasible_edge`); for a `Block`
    /// only the simple case of a single tail expression is modelled
    /// (anything richer falls through to `Unevaluated`); for everything
    /// else the result is `Unevaluated`.
    fn expr_to_lattice(&self, expr: &TirExpr) -> Lattice {
        match &expr.kind {
            TirExprKind::BoolLiteral(b) => Lattice::Const(Value::Bool(*b)),
            TirExprKind::CharLiteral(c) => Lattice::Const(Value::Char(*c)),
            TirExprKind::IntLiteral { value, .. } => {
                let Some(prim) = prim_of(expr.type_id, self.type_table).filter(|p| is_int_prim(*p))
                else {
                    return Lattice::Unevaluated;
                };
                Lattice::Const(Value::Int {
                    value: *value,
                    prim,
                })
            }
            TirExprKind::FloatLiteral { value, .. } => {
                let prim = if is_f32_type(expr.type_id, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Lattice::Const(Value::Float {
                    value: *value,
                    prim,
                })
            }
            TirExprKind::Local { index, .. } => {
                self.env.get(index).copied().unwrap_or(Lattice::Unevaluated)
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &inner.kind {
                // `outer.f` where `outer` is a plain local is the only
                // shape `field_env` indexes; nested field access
                // (`outer.inner.f`) and `(*p).f` stay `Unevaluated`.
                TirExprKind::Local { index, .. } => self
                    .field_env
                    .get(&(*index, field_name.clone()))
                    .copied()
                    .map_or(Lattice::Unevaluated, Lattice::Const),
                _ => Lattice::Unevaluated,
            },
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name),
            TirExprKind::Block(b) => self.block_lattice(b),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.expr_to_lattice(condition);
                match cond {
                    // Feasible edge: only the chosen arm contributes.
                    // The unchosen arm is unreachable and its value
                    // (whatever it is) does not enter the join.
                    Lattice::Const(Value::Bool(true)) => self.block_lattice(then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice(eb),
                        None => Lattice::Unevaluated,
                    },
                    // Non-constant / unevaluated / non-bool condition:
                    // both edges are feasible, so the result is the join
                    // of the arms' values. An arm whose `block_lattice`
                    // came back as `Unevaluated` is *not* an infeasible
                    // edge here — the arm IS reachable, we just couldn't
                    // analyze it. Promote Unevaluated → NonConst before
                    // joining so the absent information correctly pushes
                    // the merged lattice up to Top.
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice(then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => arm_lattice_for_feasible_join(self.block_lattice(eb)),
                            // No else arm: the if has type Unit, which
                            // has no representable Const value but the
                            // arm IS reachable.
                            None => Lattice::NonConst,
                        };
                        then_lat.join(else_lat)
                    }
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.match_lattice(scrutinee, arms),
            _ => Lattice::Unevaluated,
        }
    }

    /// Lattice value of a `match` expression, mirroring the [`If`] rules:
    ///
    /// - When the scrutinee is `Const(v)`, walk arms in source order. The
    ///   first arm whose pattern provably matches (and has no guard, since
    ///   guards inspect bindings tiri does not yet model) contributes its
    ///   body's lattice to the result; later arms are SCCP-infeasible
    ///   edges and never participate.
    /// - When an earlier arm is `Unknown` (an unmodelled pattern, an
    ///   unanalyzable `ConstantValue`, or a guarded arm), we cannot prove
    ///   it doesn't fire — so we conservatively treat every later arm
    ///   from that point on as also feasible, and join them all.
    /// - When the scrutinee is non-constant, every arm body is feasible:
    ///   join all of them, promoting `Unevaluated` arm values to
    ///   `NonConst` first (the same fix as the `If` non-const-condition
    ///   path — an arm we couldn't analyze is reachable, not infeasible).
    fn match_lattice(&self, scrutinee: &TirExpr, arms: &[TirMatchArm]) -> Lattice {
        let scrut_const = self.expr_to_lattice(scrutinee).as_const();

        // No-arm match shouldn't be syntactically possible, but guard
        // defensively: nothing reachable, nothing to say.
        if arms.is_empty() {
            return Lattice::Unevaluated;
        }

        if let Some(scrut_v) = scrut_const {
            // Const scrutinee: walk arms collecting candidates from
            // the first Unknown onward (which is also the first
            // arm we can't rule out) until we hit a definite Yes (or
            // run out of arms).
            let mut candidates = Vec::<Lattice>::new();
            let mut yes_found = false;
            for arm in arms {
                let pm = if arm.guard.is_some() {
                    // A guard's outcome depends on bindings we don't
                    // model; even if the pattern's structural match is
                    // definite, the arm's firing isn't. Treat as
                    // Unknown.
                    PatternMatch::Unknown
                } else {
                    self.pattern_matches(&scrut_v, &arm.pattern)
                };
                let body_lat = arm_lattice_for_feasible_join(self.expr_to_lattice(&arm.body));
                match pm {
                    PatternMatch::No => {}
                    PatternMatch::Yes => {
                        if candidates.is_empty() {
                            // Clean feasible-edge: only this arm's body
                            // value flows out. Use the un-promoted
                            // lattice — `Unevaluated` here means the
                            // body really is unanalyzable, mirroring
                            // the `If` const-cond rule.
                            return self.expr_to_lattice(&arm.body);
                        }
                        // Earlier Unknown arms could also fire; this
                        // Yes arm is the last possibility. Include it
                        // in the join and stop — no later arm is
                        // reachable past a guaranteed match.
                        candidates.push(body_lat);
                        yes_found = true;
                        break;
                    }
                    PatternMatch::Unknown => candidates.push(body_lat),
                }
            }
            // Without a proven Yes, the runtime may fall through every
            // arm and trap on the lowering's Unreachable fallback. The
            // SCCP value lattice over only the arm bodies would silently
            // drop that observable trap when a caller (e.g. the `if`
            // both-arms-equal collapse) acts on the resulting `Const`.
            // Bail to NonConst so downstream rewrites stay safe.
            if !yes_found {
                return Lattice::NonConst;
            }
            join_all(&candidates)
        } else {
            // Non-const scrutinee: every arm body is reachable. The
            // implicit Unreachable fallback is reachable too unless the
            // match is provably exhaustive — without an unguarded
            // catch-all (or pattern set covering the type's domain) we
            // cannot prove the trap is dead, and a `Const(v)` lattice
            // here would let other passes drop it. Stay conservative.
            if !is_provably_exhaustive(arms) {
                return Lattice::NonConst;
            }
            let mut acc = Lattice::Unevaluated;
            for arm in arms {
                let body_lat = arm_lattice_for_feasible_join(self.expr_to_lattice(&arm.body));
                // A guard makes the arm's *firing* uncertain, but if it
                // does fire, its body is what flows out — so the body
                // lattice still participates in the join.
                acc = acc.join(body_lat);
            }
            acc
        }
    }

    /// Decide whether `pat` matches the constant scrutinee `value`.
    /// Returns `Unknown` for any pattern shape Phase A doesn't model.
    fn pattern_matches(&self, value: &Value, pat: &TirPattern) -> PatternMatch {
        match pat {
            TirPattern::Wildcard => PatternMatch::Yes,
            TirPattern::Literal(lit) => match (lit, value) {
                (TirLiteralPattern::I128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_i128(*v, *prim, *p))
                }
                (TirLiteralPattern::U128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_u128(*v, *prim, *p))
                }
                (TirLiteralPattern::Bool(p), Value::Bool(v)) => bool_to_match(p == v),
                (TirLiteralPattern::Char(p), Value::Char(v)) => bool_to_match(p == v),
                // Type mismatch between pattern and value: definite No.
                // (The resolver should already reject ill-typed
                // patterns; if one slips through, returning No is safe
                // since the arm cannot fire at runtime either.)
                (
                    TirLiteralPattern::I128(_)
                    | TirLiteralPattern::U128(_)
                    | TirLiteralPattern::Bool(_)
                    | TirLiteralPattern::Char(_),
                    _,
                ) => PatternMatch::No,
                // String / Null patterns: tiri's `Value` doesn't carry
                // string/null info, so we can't decide. Unknown leaves
                // the arm in play.
                (TirLiteralPattern::String(_) | TirLiteralPattern::Null, _) => {
                    PatternMatch::Unknown
                }
            },
            TirPattern::Or(alts) => {
                let mut any_unknown = false;
                for alt in alts {
                    match self.pattern_matches(value, alt) {
                        PatternMatch::Yes => return PatternMatch::Yes,
                        PatternMatch::No => {}
                        PatternMatch::Unknown => any_unknown = true,
                    }
                }
                if any_unknown {
                    PatternMatch::Unknown
                } else {
                    PatternMatch::No
                }
            }
            TirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => match value {
                Value::Int { value: v, prim } => bool_to_match(range_matches_int(
                    *v,
                    *prim,
                    *start,
                    *end,
                    *inclusive,
                    *is_unsigned,
                )),
                Value::Char(c) => {
                    let cp = i128::from(u32::from(*c));
                    bool_to_match(if *inclusive {
                        cp >= *start && cp <= *end
                    } else {
                        cp >= *start && cp < *end
                    })
                }
                _ => PatternMatch::No,
            },
            TirPattern::ConstantValue { expr } => match self.expr_to_lattice(expr).as_const() {
                Some(v) if &v == value => PatternMatch::Yes,
                Some(_) => PatternMatch::No,
                None => PatternMatch::Unknown,
            },
            // Phase A out-of-scope patterns. Treat as Unknown so they
            // never wrongly commit a match (Yes) and never wrongly drop
            // a later arm (No).
            TirPattern::Binding { .. }
            | TirPattern::Tuple(_, _)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::Struct { .. } => PatternMatch::Unknown,
        }
    }

    /// Lattice value of a block: only the simple shape — a block whose
    /// sole stmt is a tail `Expr(e)` — is modelled. Such a block
    /// evaluates to whatever `e` evaluates to, so we recurse through
    /// `expr_to_lattice`. Empty blocks evaluate to `()`, which has no
    /// representable [`Value`], so they map to `Unevaluated` (the join
    /// with any other arm then carries the other arm's value out,
    /// matching the desired SCCP feasible-edge behavior). Everything
    /// else (intermediate `let` / `Assign` / `Loop` / function calls)
    /// is conservatively `Unevaluated` rather than `NonConst` so that
    /// the surrounding `if` stays foldable when the *other* arm is a
    /// constant — an arm we couldn't evaluate is treated like an
    /// infeasible edge, not a contradicting Const.
    fn block_lattice(&self, block: &TirBlock) -> Lattice {
        match block.stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &single.kind {
                TirStmtKind::Expr(e) => self.expr_to_lattice(e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// Try to fold a Binary / Unary / Cast node by looking up each
    /// operand's lattice value (literal or env-resolved local). The
    /// returned lattice mirrors operand state: any `Unevaluated` /
    /// `NonConst` operand short-circuits the result, and an op-level
    /// failure (div-by-zero, NaN, unsupported pair) is `NonConst`.
    fn try_fold(&self, expr: &TirExpr) -> Lattice {
        match &expr.kind {
            TirExprKind::Binary { left, op, right } => {
                let l = match self.expr_to_lattice(left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                let r = match self.expr_to_lattice(right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            TirExprKind::Unary { op, expr: inner } => {
                let v = match self.expr_to_lattice(inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            TirExprKind::Cast { expr: inner, .. } => {
                let Some(target) = prim_of(expr.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                // Resolve the cast input via the lattice; literal leaves
                // collapse to `Const(_)` directly, env-resolved locals
                // fall through the same path. `eval_cast` decides which
                // (source, target) pairs are foldable; unsupported pairs
                // (e.g. casts targeting i128/v128, or a target the
                // resolver should already have rejected) return `None`
                // and surface as `NonConst` rather than fabricating a
                // bogus payload.
                match self.expr_to_lattice(inner) {
                    Lattice::Const(v) => option_to_lattice(eval_cast(v, target)),
                    other => other,
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// Look up a `(module_source, name)` global in the installed
    /// [`GlobalEnv`]. Absent keys default to [`Lattice::Unevaluated`]
    /// — the engine simply has no information, same convention as
    /// un-bound locals.
    ///
    /// `IndexMap` lookup needs an owned tuple key, so each call clones
    /// `ModuleSource` (one `String` allocation per variant) and the
    /// global name. If profiling shows this on a hot path, switch the
    /// env to `IndexMap<ModuleSource, IndexMap<String, Lattice>>` or
    /// implement `Borrow`-keyed lookup; it's left flat for now since
    /// `GlobalVarGet` nodes are sparse compared to local reads.
    fn global_lattice(&self, module_source: &ModuleSource, name: &str) -> Lattice {
        let Some(globals) = self.globals else {
            return Lattice::Unevaluated;
        };
        globals
            .get(&(module_source.clone(), name.to_string()))
            .copied()
            .unwrap_or(Lattice::Unevaluated)
    }

    /// Pure-call inlining. Attempts to fold a `Call` node whose args
    /// all reduce to constants and whose callee is registered in the
    /// [`CalleeMap`].
    ///
    /// Returns `Const(v)` only when the callee body's tail expression
    /// fully reduces to a primitive [`Value`] under the bound args.
    /// Every other outcome — non-`Call` node, missing callee, non-const
    /// arg, recognized-but-unfoldable body, recursion, budget
    /// exhaustion — returns `Unevaluated` so the caller leaves the
    /// original Call in place. `NonConst` is intentionally avoided
    /// here: the call may still trap at runtime (a body whose tail
    /// folds to `NonConst` because of, say, runtime div-by-zero), and
    /// representing that as `NonConst` would let some surrounding
    /// rewrite (e.g. an `if` both-arms-equal collapse rooted on the
    /// other arm) drop the Call's evaluation.
    ///
    /// The recognized body shape is intentionally minimal — a single
    /// statement that is either `Return { Some(expr) }` or `Expr(expr)`.
    /// This covers the high-value targets (`fn double(x) { return x*2 }`,
    /// expression-bodied helpers, single-tail-`if` bodies). Multi-stmt
    /// bodies (let-sequences, multi-return) are out of scope today;
    /// bailing here costs an optimization, not correctness.
    ///
    /// Recursion is bounded by two complementary guards:
    /// `try_borrow` on the callee `RefCell` blocks the case where the
    /// visitor is currently holding `borrow_mut` (the function being
    /// walked); `call_stack` blocks CTFE-internal re-entry into a
    /// callee whose body we are already evaluating, since `try_borrow`
    /// permits concurrent immutable borrows.
    fn try_call_fold(&mut self, expr: &TirExpr) -> Lattice {
        let Some(callees) = self.callees else {
            return Lattice::Unevaluated;
        };
        let TirExprKind::Call { func, args, .. } = &expr.kind else {
            return Lattice::Unevaluated;
        };
        // Synthesise the lookup key only after we know a CalleeMap is
        // installed and the node is actually a Call — `full_name()`
        // formats a fresh String, so the order saves an allocation
        // per visited non-Call expression on the no-fold paths.
        let key: CalleeKey = (func.module_source.clone(), func.full_name());
        let Some(callee_rc) = callees.get(&key) else {
            return Lattice::Unevaluated;
        };

        // Recursion guard: refuse re-entry to a callee already being
        // evaluated. Cheaper than the borrow attempt and catches CTFE
        // recursion that `try_borrow` doesn't (multiple immutable
        // borrows are allowed).
        if self.call_stack.iter().any(|k| k == &key) {
            return Lattice::Unevaluated;
        }

        // `try_borrow` failing means the function is currently held
        // under the visitor's outer `borrow_mut`. Bail rather than
        // panic.
        let Ok(callee) = callee_rc.try_borrow() else {
            return Lattice::Unevaluated;
        };

        // Reduce every arg to a Value. We only attempt the fold when
        // every parameter has a known constant — partial constant
        // propagation into a callee is a future extension.
        let mut bound: Vec<Value> = Vec::with_capacity(args.len());
        for arg in args {
            match self.expr_to_lattice(&arg.expr).as_const() {
                Some(v) => bound.push(v),
                None => return Lattice::Unevaluated,
            }
        }

        // Param/arg arity must agree. The resolver enforces this, but
        // an arity mismatch here would silently bind the wrong locals.
        if bound.len() != callee.params.len() {
            return Lattice::Unevaluated;
        }

        // Recognize the body shape. A miss here is the engine declining
        // to model anything more involved, not a hard failure.
        let Some(tail) = single_tail_expression(&callee) else {
            return Lattice::Unevaluated;
        };

        // Charge one step per call entry. Bail (without consuming
        // anything) when exhausted so a chain that exactly hits the
        // ceiling still has its outermost result left intact rather
        // than half-folded.
        if self.step_budget == 0 {
            return Lattice::Unevaluated;
        }
        self.step_budget -= 1;

        // Push call frame, swap env to a fresh one bound to the
        // arguments. Local indices `0..params.len()` shadow the
        // parameters — the same convention the rest of the compiler
        // uses (`TirFunction::locals[0..params.len()]`).
        self.call_stack.push(key);
        let saved_env = std::mem::take(&mut self.env);
        for (i, v) in bound.iter().enumerate() {
            // u32 cast is safe: param counts are bounded by Wasm local
            // index limits, well under u32::MAX.
            #[allow(clippy::cast_possible_truncation)]
            self.env.insert(i as u32, Lattice::Const(*v));
        }

        // Reduce the tail. We use `reduce_to_lattice`, not the bare
        // `expr_to_lattice` projection, so Binary / Unary / Cast
        // inside the body actually fold against the bound env (the
        // projection alone returns Unevaluated for those kinds — only
        // `try_fold` walks them). `reduce_to_lattice` clones internally,
        // so the body inside the still-held `Ref` is not mutated.
        let result = self.reduce_to_lattice(tail);

        // Restore. The `Ref` (and its dynamic borrow on the callee
        // RefCell) drops when this scope ends.
        self.env = saved_env;
        self.call_stack.pop();

        // Only Const(v) is exposed to the caller. NonConst from the
        // tail (e.g. a Const/Const op that hit a runtime trap like
        // div-by-zero inside the body) is downgraded to Unevaluated
        // so the original Call expression is left intact and the
        // runtime trap survives.
        match result {
            c @ Lattice::Const(_) => c,
            Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
        }
    }
}

/// Recognize a callee body shape the engine can evaluate: a single
/// statement that is either `Return { Some(expr) }` or `Expr(expr)`.
/// Returns the tail expression in either case.
///
/// Anything else (zero or multiple stmts, intermediate Let / If / Loop /
/// Break / Return without value, …) reports `None`. The caller treats
/// `None` as "do not fold this call", preserving the runtime call.
fn single_tail_expression(func: &TirFunction) -> Option<&TirExpr> {
    let body = func.body.as_ref()?;
    let [single] = body.stmts.as_slice() else {
        return None;
    };
    match &single.kind {
        TirStmtKind::Return { value: Some(e) } | TirStmtKind::Expr(e) => Some(e),
        _ => None,
    }
}

/// `Some(v)` ↦ `Const(v)`, `None` ↦ `NonConst`. Used at the boundary
/// where a numeric-evaluation helper that still returns `Option<Value>`
/// (because its failure modes are runtime traps, not "haven't tried")
/// flows back into the lattice surface.
fn option_to_lattice(opt: Option<Value>) -> Lattice {
    match opt {
        Some(v) => Lattice::Const(v),
        None => Lattice::NonConst,
    }
}

/// Outcome of testing a [`TirPattern`] against a constant scrutinee
/// [`Value`]. The three states mirror the pattern's contribution to
/// SCCP feasibility in [`Interpreter::match_lattice`]:
///
/// - `Yes` — the pattern provably matches; later arms are infeasible
///   edges.
/// - `No` — the pattern provably does not match; this arm is an
///   infeasible edge.
/// - `Unknown` — the engine cannot decide (an unmodelled pattern
///   shape, a guard the engine doesn't analyze, a `ConstantValue`
///   whose inner expression doesn't reduce). The arm stays in play
///   and contributes to the join with all later arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternMatch {
    Yes,
    No,
    Unknown,
}

fn bool_to_match(b: bool) -> PatternMatch {
    if b {
        PatternMatch::Yes
    } else {
        PatternMatch::No
    }
}

/// Conservatively decide whether a `match`'s arm set is exhaustive
/// — i.e. whether the lowering's implicit `Unreachable` fallback is
/// provably dead. Mirrors the resolver's [`is_catch_all_pattern`]
/// rule: at least one unguarded `Wildcard` / `Binding` arm (or an
/// `Or` pattern containing one) is sufficient. Variant-set / range-set
/// coverage proofs are deferred until tiri models those pattern shapes
/// structurally; treating them as non-exhaustive here is the safe
/// answer (it costs an optimization, not correctness).
fn is_provably_exhaustive(arms: &[TirMatchArm]) -> bool {
    arms.iter()
        .any(|a| a.guard.is_none() && pattern_is_catch_all(&a.pattern))
}

fn pattern_is_catch_all(pat: &TirPattern) -> bool {
    match pat {
        TirPattern::Wildcard | TirPattern::Binding { .. } => true,
        TirPattern::Or(alts) => alts.iter().any(pattern_is_catch_all),
        _ => false,
    }
}

/// Join a slice of lattice values via [`Lattice::join`]. Empty input
/// returns [`Lattice::Unevaluated`] (the join's identity).
fn join_all(lats: &[Lattice]) -> Lattice {
    let mut acc = Lattice::Unevaluated;
    for &l in lats {
        acc = acc.join(l);
    }
    acc
}

/// Compare an integer value (raw bits + prim) against a signed i128
/// pattern literal. Returns `true` iff the values are equal under
/// the prim's signedness interpretation.
fn int_value_matches_i128(value: u64, prim: PrimitiveType, pat: i128) -> bool {
    let Some(v) = int_value_as_i128(value, prim) else {
        return false;
    };
    v == pat
}

/// Compare an integer value (raw bits + prim) against an unsigned
/// u128 pattern literal.
fn int_value_matches_u128(value: u64, prim: PrimitiveType, pat: u128) -> bool {
    if is_signed_int(prim) {
        // Signed value cannot represent values outside i64 range
        // anyway; reinterpret as unsigned for comparison.
        let v = value as i64;
        if v < 0 {
            return false;
        }
        u128::from(v as u64) == pat
    } else {
        u128::from(value) == pat
    }
}

/// Convert a (raw bits, prim) integer into an i128, sign- or
/// zero-extending per the prim's signedness. Returns `None` for
/// non-integer prims.
fn int_value_as_i128(value: u64, prim: PrimitiveType) -> Option<i128> {
    if !is_int_prim(prim) {
        return None;
    }
    if is_signed_int(prim) {
        // Stored as sign-extended i64 → widen to i128.
        Some(i128::from(value as i64))
    } else {
        Some(i128::from(value))
    }
}

/// Decide whether a (raw bits, prim) integer falls inside a range
/// pattern. Returns `false` for non-integer prims and for negative
/// signed values against an unsigned-typed range (which by
/// construction starts at zero or higher); otherwise returns the
/// usual half-open / closed range membership test in i128 space.
fn range_matches_int(
    value: u64,
    prim: PrimitiveType,
    start: i128,
    end: i128,
    inclusive: bool,
    is_unsigned_pat: bool,
) -> bool {
    if !is_int_prim(prim) {
        return false;
    }
    let v: i128 = if is_unsigned_pat || !is_signed_int(prim) {
        // Treat the value as unsigned. For a signed prim with negative
        // bits, the unsigned reinterpretation differs — fall back to
        // sign-extended comparison, then ensure it stays nonneg before
        // entering an unsigned range check.
        if is_signed_int(prim) {
            let signed = i128::from(value as i64);
            if signed < 0 {
                // The pattern is unsigned; a negative scrutinee can't
                // be in `[start, end]` when start ≥ 0.
                return false;
            }
            signed
        } else {
            i128::from(value)
        }
    } else {
        i128::from(value as i64)
    };
    if inclusive {
        v >= start && v <= end
    } else {
        v >= start && v < end
    }
}

/// Adjust a block's raw lattice value before feeding it into an
/// arm-feasible-join (the `if` non-constant-condition path).
///
/// `Lattice::Unevaluated` from `block_lattice` means "we couldn't
/// analyze this block's value" — which is fine when the block is the
/// chosen branch of a constant-condition `if` (the other arm is an
/// infeasible edge, so the result really is "we don't know"), but
/// becomes unsound under a non-constant condition: that arm is
/// reachable, the absence of a known value means SCCP-Top
/// (`NonConst`), not infeasibility. Promote here so that a subsequent
/// `Lattice::join` cannot let an `Unevaluated` arm be silently
/// absorbed by a `Const` peer (`join(Unevaluated, Const(v)) → Const(v)`
/// is the infeasible-edge rule, valid only when the Unevaluated edge
/// really is unreachable).
fn arm_lattice_for_feasible_join(lat: Lattice) -> Lattice {
    match lat {
        Lattice::Unevaluated => Lattice::NonConst,
        other => other,
    }
}

/// Conservative effect-free check for an expression that we may want
/// to drop entirely (specifically, the condition of an `if` whose two
/// arms reduce to the same lattice constant). "Effect-free" here means
/// "evaluating this neither traps, mutates, nor calls a function that
/// might do either". The check is structural and only admits a small
/// allow-list — when in doubt, stay conservative and refuse the rewrite.
///
/// Notes:
///
/// - `Binary { Div | Mod, … }` is excluded because integer
///   division-by-zero traps; `Unary { Deref, … }` is excluded for the
///   same reason (a null/invalid reference would trap).
/// - `FieldAccess` over speculatable receivers is allowed: a non-null
///   GC reference's field load cannot trap once we have the reference.
/// - Calls of any flavor are rejected — even pure callees may return
///   different values across invocations (e.g. `random.next()` is
///   marked pure-by-effect in Wado but is not idempotent in the SCCP
///   sense), and tiri does not yet inline pure calls.
fn is_speculatable(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::Unit => true,
        TirExprKind::Binary { left, op, right } => {
            !matches!(op, TirBinaryOp::Div | TirBinaryOp::Mod)
                && is_speculatable(left)
                && is_speculatable(right)
        }
        TirExprKind::Unary { op, expr: inner } => {
            !matches!(op, TirUnaryOp::Deref) && is_speculatable(inner)
        }
        TirExprKind::Cast { expr: inner, .. } => is_speculatable(inner),
        TirExprKind::FieldAccess { expr: inner, .. } => is_speculatable(inner),
        _ => false,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TirExpr <-> Value bridge
// ──────────────────────────────────────────────────────────────────────────────

fn value_to_expr_kind(v: Value) -> TirExprKind {
    match v {
        Value::Int { value, prim } => TirExprKind::IntLiteral {
            repr: format_int_repr(value, prim),
            value,
        },
        Value::Float { value, .. } => TirExprKind::FloatLiteral {
            repr: format_float_repr(value),
            value,
        },
        Value::Bool(b) => TirExprKind::BoolLiteral(b),
        Value::Char(c) => TirExprKind::CharLiteral(c),
    }
}

/// Identity simplifications for short-circuit operators that *preserve*
/// every subexpression. `false || X → X`, `true && X → X`, and the RHS
/// counterparts (`X || false → X`, `X && true → X`). Returns `true`
/// when `expr` was rewritten.
///
/// The reverse direction (`true || X → true`, `false && X → false`)
/// would drop `X`. Even though Wado's `||`/`&&` short-circuit at runtime
/// — so dropping a side that wouldn't have been evaluated is
/// semantically defensible — this engine stays conservative and leaves
/// those rewrites to a future side-effect-aware pass. Mirrors the
/// previous in-visitor behaviour.
fn rewrite_short_circuit(expr: &mut TirExpr) -> bool {
    enum Pick {
        Left,
        Right,
    }
    let pick = match &expr.kind {
        TirExprKind::Binary { left, op, right } => match (&left.kind, *op, &right.kind) {
            (TirExprKind::BoolLiteral(false), TirBinaryOp::Or, _)
            | (TirExprKind::BoolLiteral(true), TirBinaryOp::And, _) => Pick::Right,
            (_, TirBinaryOp::Or, TirExprKind::BoolLiteral(false))
            | (_, TirBinaryOp::And, TirExprKind::BoolLiteral(true)) => Pick::Left,
            _ => return false,
        },
        _ => return false,
    };
    // Take ownership of the Binary by swapping its `kind` out. The
    // placeholder is local to this function and overwritten before we
    // return, so no caller observes a partially-updated `expr`.
    let TirExprKind::Binary { left, right, .. } =
        std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        unreachable!("matched Binary above");
    };
    *expr = match pick {
        Pick::Left => *left,
        Pick::Right => *right,
    };
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure value evaluation (Bool / Int / Float)
// ──────────────────────────────────────────────────────────────────────────────

/// Evaluate a binary op on two compile-time values.
fn eval_binary(left: Value, op: TirBinaryOp, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Bool(l), Value::Bool(r)) => eval_bool_binary(l, op, r),
        (Value::Char(l), Value::Char(r)) => eval_char_binary(l, op, r),
        (Value::Float { value: l, prim: lp }, Value::Float { value: r, prim: rp }) if lp == rp => {
            eval_float_binary(l, op, r, lp)
        }
        (Value::Int { value: l, prim: lp }, Value::Int { value: r, prim: rp }) if lp == rp => {
            eval_int_binary(l, op, r, lp)
        }
        _ => None,
    }
}

/// Evaluate a unary op on a compile-time value.
fn eval_unary(op: TirUnaryOp, operand: Value) -> Option<Value> {
    match op {
        TirUnaryOp::Neg => match operand {
            Value::Int { value, prim } => {
                eval_int_neg(value, prim).map(|v| Value::Int { value: v, prim })
            }
            Value::Float { value, prim } => {
                let negated = f64::from_bits(value.to_bits() ^ (1u64 << 63));
                Some(Value::Float {
                    value: negated,
                    prim,
                })
            }
            Value::Bool(_) | Value::Char(_) => None,
        },
        TirUnaryOp::Not => match operand {
            Value::Bool(b) => Some(Value::Bool(!b)),
            _ => None,
        },
        TirUnaryOp::BitNot => match operand {
            Value::Int { value, prim } => Some(Value::Int {
                value: truncate_int(!value, prim),
                prim,
            }),
            _ => None,
        },
        TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => None,
    }
}

/// Evaluate an `as` cast at compile time.
///
/// Source values are the lattice-resolved [`Value`] of the cast input;
/// `target` is the destination primitive (resolved from the cast node's
/// `type_id`). Returns `None` for unsupported pairs — the caller maps
/// that to [`Lattice::NonConst`] so the runtime cast still happens, no
/// bogus value gets folded in.
///
/// The supported set mirrors what the resolver permits in source:
///
/// - `Int` source ↦ Int (already supported), Float, Char (only when
///   source is `U8` per [`expr.rs`]'s `u8 as char` carve-out).
/// - `Float` source ↦ Float, Int (saturating, matching Wasm's
///   `*.trunc_sat_*` semantics — Rust's `as` since 1.45 implements the
///   same rounding/saturation rules so we forward to it).
/// - `Bool` source ↦ Int (0/1), Float (0.0/1.0). Bool → Bool is the
///   identity.
/// - `Char` source ↦ Int (codepoint, then truncated). Char → Char is the
///   identity.
///
/// 128-bit (`I128`/`U128`) and SIMD (`V128`) targets are reachable here
/// (they are valid `Primitive` variants) but currently unsupported and
/// fall through to `None`.
fn eval_cast(source: Value, target: PrimitiveType) -> Option<Value> {
    let int_target = is_int_prim(target);
    let float_target = matches!(target, PrimitiveType::F32 | PrimitiveType::F64);
    match source {
        // The source `prim` is irrelevant for int→int because
        // `truncate_int` operates on the already sign- or zero-extended
        // u64 representation set up at construction time.
        Value::Int { value, .. } if int_target => Some(Value::Int {
            value: truncate_int(value, target),
            prim: target,
        }),
        Value::Int { value, prim } if float_target => Some(int_to_float(value, prim, target)),
        // Only `u8 as char` is permitted by the resolver; every u8 is a
        // valid Unicode scalar, so `char::from(u8)` is total.
        Value::Int {
            value,
            prim: PrimitiveType::U8,
        } if target == PrimitiveType::Char => Some(Value::Char(char::from(value as u8))),

        Value::Float { value, prim } if float_target => Some(float_to_float(value, prim, target)),
        Value::Float { value, prim } if int_target => Some(float_to_int(value, prim, target)),

        Value::Bool(b) if int_target => Some(Value::Int {
            value: u64::from(b),
            prim: target,
        }),
        Value::Bool(b) if float_target => Some(Value::Float {
            value: if b { 1.0 } else { 0.0 },
            prim: target,
        }),
        Value::Bool(b) if target == PrimitiveType::Bool => Some(Value::Bool(b)),

        Value::Char(c) if int_target => Some(Value::Int {
            value: truncate_int(u64::from(c as u32), target),
            prim: target,
        }),
        Value::Char(c) if target == PrimitiveType::Char => Some(Value::Char(c)),

        _ => None,
    }
}

/// True for the eight integer primitives the engine models. 128-bit
/// (`I128`/`U128`) is intentionally excluded — those types lower to
/// stdlib calls in source, not a `Cast` node tiri can fold.
fn is_int_prim(p: PrimitiveType) -> bool {
    matches!(
        p,
        PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64,
    )
}

/// Convert an integer (held as the sign-extended u64 bit pattern of
/// `prim`) into a float of `target` width. Signed widths are routed
/// through `i64` so the negative range survives; unsigned widths use
/// `u64` directly. F32 results are widened back to f64 so the engine's
/// canonical [`Value::Float`] repr is preserved.
fn int_to_float(value: u64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let f = if is_signed_int(prim) {
        // `truncate_int` already sign-extended `value` into the i64 range
        // for I8/I16/I32, and an I64 value's u64 bits round-trip through
        // `as i64 as f64`.
        match target {
            PrimitiveType::F32 => f64::from((value as i64) as f32),
            _ => (value as i64) as f64,
        }
    } else {
        match target {
            PrimitiveType::F32 => f64::from(value as f32),
            _ => value as f64,
        }
    };
    Value::Float {
        value: f,
        prim: target,
    }
}

/// Float ↔ float conversion. Widening (f32 → f64) is a no-op on the
/// stored f64 since every f32 is exactly representable; narrowing
/// (f64 → f32) routes through `as f32` to apply the rounding step,
/// then re-widens to f64 for storage. Same-width casts are the identity.
fn float_to_float(value: f64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let v = match (prim, target) {
        (PrimitiveType::F64, PrimitiveType::F32) => f64::from(value as f32),
        (PrimitiveType::F32 | PrimitiveType::F64, PrimitiveType::F64)
        | (PrimitiveType::F32, PrimitiveType::F32) => value,
        _ => panic!("float_to_float: non-float prim ({prim:?} → {target:?})"),
    };
    Value::Float {
        value: v,
        prim: target,
    }
}

/// Float → integer with Wasm `trunc_sat` semantics: NaN ↦ 0, ±∞ saturate
/// to the target's MIN/MAX, finite values truncate toward zero with
/// saturation. Rust's `as` since 1.45 matches this exactly, so we
/// dispatch through it for the source/target widths that map directly.
///
/// Caller guarantees `target` is one of the i8..u64 primitives (the
/// dispatch in [`eval_cast`] enforces this); panics otherwise to flag
/// a bug rather than fabricate a zero.
fn float_to_int(value: f64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    // For F32 sources the stored f64 is bit-equivalent to the original
    // f32, but the truncation must be performed at f32 precision to
    // match the runtime cast — large magnitudes saturate sooner. Cast
    // back through f32 first when needed; otherwise the f64 path is a
    // no-op widening and the same code computes the answer.
    let raw = match prim {
        PrimitiveType::F32 => trunc_sat_to_int(f64::from(value as f32), target),
        _ => trunc_sat_to_int(value, target),
    };
    Value::Int {
        value: truncate_int(raw, target),
        prim: target,
    }
}

/// Saturating float → int conversion, dispatched by target width.
/// Operates on f64 since every f32 fits exactly; the caller is
/// responsible for narrowing to f32 precision first when the source
/// type was F32.
fn trunc_sat_to_int(value: f64, target: PrimitiveType) -> u64 {
    match target {
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value as i64 as u64,
        PrimitiveType::U8 => u64::from(value as u8),
        PrimitiveType::U16 => u64::from(value as u16),
        PrimitiveType::U32 => u64::from(value as u32),
        PrimitiveType::U64 => value as u64,
        _ => panic!("trunc_sat_to_int: non-integer target {target:?}"),
    }
}

fn eval_bool_binary(l: bool, op: TirBinaryOp, r: bool) -> Option<Value> {
    match op {
        TirBinaryOp::And => Some(Value::Bool(l && r)),
        TirBinaryOp::Or => Some(Value::Bool(l || r)),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        // bool implements Ord with `false < true`. Spelled with `&&`
        // rather than `<` to satisfy clippy's `bool_comparison` lint
        // without tripping `needless_bitwise_bool`.
        TirBinaryOp::Lt => Some(Value::Bool(!l && r)),
        TirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        TirBinaryOp::Gt => Some(Value::Bool(l && !r)),
        TirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

/// `char` comparisons. char implements `Eq` and `Ord` (codepoint
/// order); arithmetic / bitwise ops are not defined.
fn eval_char_binary(l: char, op: TirBinaryOp, r: char) -> Option<Value> {
    match op {
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        TirBinaryOp::Lt => Some(Value::Bool(l < r)),
        TirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        TirBinaryOp::Gt => Some(Value::Bool(l > r)),
        TirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_int_binary(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> Option<Value> {
    match op {
        TirBinaryOp::Add => Some(Value::Int {
            value: truncate_int(lval.wrapping_add(rval), prim),
            prim,
        }),
        TirBinaryOp::Sub => Some(Value::Int {
            value: truncate_int(lval.wrapping_sub(rval), prim),
            prim,
        }),
        TirBinaryOp::Mul => Some(Value::Int {
            value: truncate_int(lval.wrapping_mul(rval), prim),
            prim,
        }),
        TirBinaryOp::Div => eval_int_div(lval, rval, prim).map(|value| Value::Int { value, prim }),
        TirBinaryOp::Mod => eval_int_mod(lval, rval, prim).map(|value| Value::Int { value, prim }),

        TirBinaryOp::Eq
        | TirBinaryOp::NotEq
        | TirBinaryOp::Lt
        | TirBinaryOp::LtEq
        | TirBinaryOp::Gt
        | TirBinaryOp::GtEq => Some(Value::Bool(eval_int_cmp(lval, op, rval, prim))),

        TirBinaryOp::BitAnd => Some(Value::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        TirBinaryOp::BitOr => Some(Value::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        TirBinaryOp::BitXor => Some(Value::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        TirBinaryOp::Shl => Some(Value::Int {
            value: eval_int_shl(lval, rval, prim),
            prim,
        }),
        TirBinaryOp::Shr => Some(Value::Int {
            value: eval_int_shr(lval, rval, prim),
            prim,
        }),

        TirBinaryOp::And | TirBinaryOp::Or | TirBinaryOp::RefEq | TirBinaryOp::RefNotEq => None,
    }
}

fn eval_int_cmp(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    if is_signed_int(prim) {
        let l = lval as i64;
        let r = rval as i64;
        match op {
            TirBinaryOp::Eq => l == r,
            TirBinaryOp::NotEq => l != r,
            TirBinaryOp::Lt => l < r,
            TirBinaryOp::LtEq => l <= r,
            TirBinaryOp::Gt => l > r,
            TirBinaryOp::GtEq => l >= r,
            _ => unreachable!(),
        }
    } else {
        match op {
            TirBinaryOp::Eq => lval == rval,
            TirBinaryOp::NotEq => lval != rval,
            TirBinaryOp::Lt => lval < rval,
            TirBinaryOp::LtEq => lval <= rval,
            TirBinaryOp::Gt => lval > rval,
            TirBinaryOp::GtEq => lval >= rval,
            _ => unreachable!(),
        }
    }
}

fn eval_int_shl(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    truncate_int(lval.wrapping_shl(shift), prim)
}

fn eval_int_shr(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    if is_signed_int(prim) {
        let result = (lval as i64).wrapping_shr(shift);
        truncate_int(result as u64, prim)
    } else {
        truncate_int(lval.wrapping_shr(shift), prim)
    }
}

fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval / rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_div(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_div(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval % rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_rem(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_rem(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_rem(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_rem(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_neg(value: u64, prim: PrimitiveType) -> Option<u64> {
    match prim {
        PrimitiveType::I8 => {
            let result = (value as i8).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (value as i16).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (value as i32).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            let result = (value as i64).wrapping_neg();
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_float_binary(lval: f64, op: TirBinaryOp, rval: f64, prim: PrimitiveType) -> Option<Value> {
    match prim {
        PrimitiveType::F32 => eval_f32_binary(lval, op, rval),
        PrimitiveType::F64 => eval_f64_binary(lval, op, rval),
        _ => None,
    }
}

fn eval_f64_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Add => non_nan_float(lval + rval, PrimitiveType::F64),
        TirBinaryOp::Sub => non_nan_float(lval - rval, PrimitiveType::F64),
        TirBinaryOp::Mul => non_nan_float(lval * rval, PrimitiveType::F64),
        TirBinaryOp::Div => non_nan_float(lval / rval, PrimitiveType::F64),
        _ => eval_float_comparison(lval, op, rval),
    }
}

fn eval_f32_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        TirBinaryOp::Add => non_nan_float(f64::from(l + r), PrimitiveType::F32),
        TirBinaryOp::Sub => non_nan_float(f64::from(l - r), PrimitiveType::F32),
        TirBinaryOp::Mul => non_nan_float(f64::from(l * r), PrimitiveType::F32),
        TirBinaryOp::Div => non_nan_float(f64::from(l / r), PrimitiveType::F32),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        TirBinaryOp::Lt => Some(Value::Bool(l < r)),
        TirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        TirBinaryOp::Gt => Some(Value::Bool(l > r)),
        TirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_float_comparison(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Eq => Some(Value::Bool(lval == rval)),
        TirBinaryOp::NotEq => Some(Value::Bool(lval != rval)),
        TirBinaryOp::Lt => Some(Value::Bool(lval < rval)),
        TirBinaryOp::LtEq => Some(Value::Bool(lval <= rval)),
        TirBinaryOp::Gt => Some(Value::Bool(lval > rval)),
        TirBinaryOp::GtEq => Some(Value::Bool(lval >= rval)),
        _ => None,
    }
}

fn non_nan_float(value: f64, prim: PrimitiveType) -> Option<Value> {
    if value.is_nan() {
        return None;
    }
    Some(Value::Float { value, prim })
}

// ──────────────────────────────────────────────────────────────────────────────
// Type queries, truncation, formatting
// ──────────────────────────────────────────────────────────────────────────────

fn is_signed_int(prim: PrimitiveType) -> bool {
    matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    )
}

fn int_bit_width(prim: PrimitiveType) -> u32 {
    match prim {
        PrimitiveType::I8 | PrimitiveType::U8 => 8,
        PrimitiveType::I16 | PrimitiveType::U16 => 16,
        PrimitiveType::I32 | PrimitiveType::U32 => 32,
        PrimitiveType::I64 | PrimitiveType::U64 => 64,
        _ => 32,
    }
}

fn is_f32_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(PrimitiveType::F32)
    )
}

/// Resolve any primitive type from a [`TypeId`]. Used by the cast path
/// where the target may be int / float / bool / char (i128/u128/v128
/// are returned but [`eval_cast`] declines to fold them) and by
/// `IntLiteral` lattice resolution after a [`is_int_prim`] filter.
fn prim_of(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(p) => Some(*p),
        _ => None,
    }
}

/// Truncate / sign-extend an integer bit pattern to fit the target prim.
#[must_use]
pub(crate) fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
    match prim {
        PrimitiveType::U8 => value & 0xFF,
        PrimitiveType::U16 => value & 0xFFFF,
        PrimitiveType::U32 => value & 0xFFFF_FFFF,
        PrimitiveType::U64 => value,
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value,
        _ => value,
    }
}

/// Render an integer bit pattern as decimal text, signed when the prim
/// is signed.
#[must_use]
pub(crate) fn format_int_repr(value: u64, prim: PrimitiveType) -> String {
    if is_signed_int(prim) {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Render a `char` as a Wado-friendly literal repr (`'A'`, `'\n'`,
/// `'\u{1F600}'`, …). Used when re-emitting a folded `char` value as a
/// `TirExprKind::CharLiteral`.
#[must_use]
pub(crate) fn format_char_repr(c: char) -> String {
    match c {
        '\\' => "'\\\\'".to_string(),
        '\'' => "'\\''".to_string(),
        '\n' => "'\\n'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\t' => "'\\t'".to_string(),
        '\0' => "'\\0'".to_string(),
        c if c.is_ascii_graphic() || c == ' ' => format!("'{c}'"),
        c => format!("'\\u{{{:X}}}'", c as u32),
    }
}

/// Render a float as a Wado-friendly literal repr (`3.25`, `0.0`,
/// `Infinity`, `-Infinity`, …). Trailing `.0` is appended to integral
/// values so the result parses back as a float literal.
#[must_use]
pub(crate) fn format_float_repr(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    let s = value.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}
