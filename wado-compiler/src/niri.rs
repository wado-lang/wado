//! NIR Interpreter (niri).
//!
//! Compile-time partial evaluator for Wado NIR, operating on the arena `Body`.
//! [`Interpreter::reduce_local_a`] rewrites one node in place toward literal
//! form, [`Interpreter::reduce_to_lattice_a`] projects a node to a [`Lattice`],
//! and [`Interpreter::reduce_in_place_a`] reduces a whole subtree bottom-up.
//! Constant folding is the primary consumer; branch pruning, constant
//! propagation, and compile-time function evaluation reuse the same engine.
//!
//! Reduction is **monotone** — it only moves expressions toward literal form,
//! never the reverse — and **idempotent**. Literal leaves are preserved as-is
//! so the original lexical repr (e.g. `0xFF`) survives a no-op pass.
//!
//! The engine handles:
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
//!   - u8 → char (the only int → char form the elaborator permits)
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
//!   Modelled patterns: `_`, a binding, integer / bool / char literal,
//!   integer range (signed and unsigned), or-of the above,
//!   `ConstantValue` whose inner expression reduces to a `Value`, and
//!   struct / exact-arity tuple patterns whose every field pattern is
//!   itself modelled. Tuple-with-rest, `Variant`, `Enum`, and string /
//!   null literal patterns report `Unknown` — they never wrongly commit
//!   a match and never wrongly drop a later arm. A definite field
//!   mismatch still rules an arm out even when a sibling field binds,
//!   and an arm's guard is decided with its bindings in scope.
//! - Struct / tuple literals: an aggregate whose every field reduces to
//!   a constant is itself a constant ([`Value::Aggregate`]), and
//!   `receiver.field` projects a field back out — including out of a
//!   CTFE-folded call result. Aggregates never leave the engine (the
//!   value pool models pure scalars), so what reaches the IR is the
//!   scalars projected out of them. A local carries an aggregate
//!   constant only when every mention of it merely reads the value; see
//!   [`Interpreter::record_aggregate_locals`].
//! - Pure-call evaluation: a free `Call` whose args all reduce to
//!   constants and whose callee was admitted to the [`CalleeMap`]
//!   (pure, non-async, monomorphic — see [`is_ctfe_eligible`]) runs the
//!   callee's body with the args bound into a fresh local environment.
//!   The body is executed statement by statement — `let` bindings,
//!   assignment to a local, a decidable `if`, an early `return`, a
//!   labeled block completed by its `break`, and a loop run until it
//!   breaks. Anything else abandons the evaluation, leaving the
//!   original call — and any runtime trap inside it — in place. The
//!   `call_stack` blocks recursive re-entry and the step budget caps
//!   total work, so a loop needs no constant trip count.
//!   `MethodCall` / `IndirectCall` / `CmRawCall` are out of scope.
//!
//! Float arithmetic uses native Rust IEEE 754 ops (same as Wasm), following
//! cranelift's approach: fold the result, but skip if it is NaN since NaN
//! bit patterns are nondeterministic across architectures.
//!
//! Integer division/modulo by zero and signed `MIN / -1` are left
//! unfolded so the runtime trap is preserved.
//!
//! See `docs/wep-2026-04-27-nir-interpreter.md` for the design.

use std::cell::RefCell;
use std::rc::Rc;

use crate::const_eval::{
    eval_binary, eval_cast, eval_unary, is_f32_type, is_int_prim, is_signed_int, prim_of,
};
// `Value` lives in `const_eval`; re-export it so `niri::Value` resolves for
// the public API and tests.
use crate::compiler_item::SeqField;
pub use crate::const_eval::{MAX_SEQ_ELEMENTS, Value};
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{NirBinaryOp, NirFunction, NirLiteralPattern, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId,
    PatKind, StmtId, StmtKind, StmtNode,
};
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{PrimitiveType, TypeId, TypeTable};

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
#[derive(Debug, Clone, PartialEq)]
pub enum Lattice {
    /// No information yet. Default for un-bound locals and NIR kinds
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
            Self::Const(v) => Some(v.clone()),
            Self::Unevaluated | Self::NonConst => None,
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

// ──────────────────────────────────────────────────────────────────────────────
// Callee map
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a callee in the [`CalleeMap`]. Mirrors the shape produced
/// by `FunctionRef::full_name` so the interpreter can look up a `Call`
/// node's target without re-deriving the format.
pub type CalleeKey = crate::nir::FuncId;

/// Map of CTFE-eligible callees, keyed by canonical [`crate::nir::FuncId`].
///
/// Values are [`Rc<RefCell<NirFunction>>`] handles aliased with
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
pub type CalleeMap = IndexMap<CalleeKey, Rc<RefCell<NirFunction>>>;

/// The array builtins the engine can evaluate over a constant sequence. An
/// element read reaches NIR as a call to one of these, not as an `Index` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqBuiltin {
    Get,
    Len,
}

/// Which sequence builtin each callee id is, for the ids that are one.
pub type SeqBuiltinMap = IndexMap<CalleeKey, SeqBuiltin>;

// ──────────────────────────────────────────────────────────────────────────────
// Global env
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a global variable in the [`GlobalEnv`]. Mirrors the
/// `(module_source, name)` shape carried by `ExprKind::GlobalVarGet`
/// so the interpreter can look up a `GlobalVarGet` node directly.
pub type GlobalKey = (ModuleSource, String);

/// Lattice values for module-scope globals.
///
/// Populated once per pass by the driving visitor from
/// [`crate::flat_package::FlatPackage::globals`] — typically by
/// reducing each non-`mut` global's initializer through a fresh
/// [`Interpreter`] (so initializers like `1 + 2`, `i32::MAX - 1`, or
/// pure-call expressions all collapse to `Const(_)`). Mutable globals
/// are mapped to [`Lattice::NonConst`] so reads through niri stay
/// conservative even while the global is in scope.
///
/// The map is read at every `GlobalVarGet` lookup; absent keys default
/// to [`Lattice::Unevaluated`] (the engine simply doesn't know — same
/// rule as un-bound locals).
pub type GlobalEnv = IndexMap<GlobalKey, Lattice>;

/// Known constant field values of module-scope globals, keyed by global then
/// field name. It lets
/// `FieldAccess(GlobalVarGet(X), f)` fold to a constant when `X` is an
/// immutable global whose `f` field is statically known — e.g. the
/// [`SeqField::Len`](crate::compiler_item::SeqField) length of an immutable
/// sequence global hoisted by body globalization.
pub type GlobalFieldEnv = IndexMap<GlobalKey, IndexMap<String, Value>>;

/// Ceiling on total CTFE work, charged per call entry, statement, and loop
/// iteration. Reset per function so what one function spends cannot decide
/// whether the next one folds.
pub const DEFAULT_STEP_BUDGET: u32 = 10_000;

// ──────────────────────────────────────────────────────────────────────────────
// Field knowledge
// ──────────────────────────────────────────────────────────────────────────────

/// A dense set of local indices, backed by a bitset indexed by the local
/// index itself.
///
/// Local indices within a function body are dense (`0..locals.len()`), so
/// this replaces an `IndexSet<u32>` used purely for membership with a
/// hash-free bitset — the same idea as [`crate::tir::TypeSet`]. The alias
/// analysis rebuilds these sets for every function on every const-fold
/// iteration, so dropping the per-grow allocation + hashing of an
/// `IndexSet` is worthwhile.
#[derive(Default, Clone, Debug)]
pub struct LocalSet {
    words: Vec<u64>,
}

impl LocalSet {
    /// An empty set pre-sized to hold `locals` indices without regrowing.
    #[must_use]
    pub fn with_capacity(locals: usize) -> Self {
        Self {
            words: vec![0; locals.div_ceil(64)],
        }
    }

    fn slot(index: u32) -> (usize, u64) {
        ((index / 64) as usize, 1u64 << (index % 64))
    }

    /// Insert `index`, returning `true` if it was not already present.
    pub fn insert(&mut self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let newly = self.words[word] & mask == 0;
        self.words[word] |= mask;
        newly
    }

    /// Whether `index` is a member.
    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        self.words.get(word).is_some_and(|w| w & mask != 0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Iterate members in ascending index order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            (0..64u32)
                .filter(move |&b| word & (1u64 << b) != 0)
                .map(move |b| wi as u32 * 64 + b)
        })
    }
}

/// Per-function alias / aliasing-trackability annotations.
///
/// Computed once per function by [`crate::optimize::alias::build_alias_info`]
/// (from the function's stable `address_taken_locals` /
/// `stores_aliased_locals` plus a body walk that catches transient inlined-in
/// copies) and consumed by the engine [`ValueGraph`] builder
/// ([`crate::optimize::alias::builder_alias_sets`]) to bound heap-write
/// invalidation at the right granularity.
///
/// - `aliased`: locals reachable through some other handle (`&x`,
///   `&mut x`, captured by a closure, struct-field-stored, etc.).
/// - `untrackable`: locals whose aliasing escapes the analysis (e.g.
///   stashed across a `stores`-annotated callee).
/// - `alias_groups`: union-find groups of locals connected by
///   reference-typed `let dst = src` copies (`Box<T>`, `List<T>`,
///   `&T`, `&mut T`).
///
/// [`ValueGraph`]: crate::nir_value_graph
#[derive(Default, Clone, Debug)]
pub struct AliasInfo {
    pub aliased: LocalSet,
    pub untrackable: LocalSet,
    pub alias_groups: IndexMap<u32, IndexSet<u32>>,
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
///
/// Commit sink for niri's body rewrites. The rewrite logic reads through
/// [`EditSink::body`] and commits every edit through the sink, so two backends
/// can share it: [`BodySink`] mutates a `Body` in place — used for throwaway
/// CTFE scratch bodies, where coherence with an engine's parent map / use
/// index is moot — while the optimize layer's `EngineSink` routes every edit
/// through `Engine::*` so the real body's maps stay coherent.
pub(crate) trait EditSink {
    fn body(&self) -> &Body;
    /// Replace `e`'s kind. The new kind's children must already be parented to
    /// `e` (literals have none); use [`EditSink::become_expr`] to move an
    /// existing node's content into `e`.
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind);
    /// Promote `e` to the folded pure scalar `value`, returning whether the edit
    /// was applied (WEP: The Live `ValueGraph`). The engine backend swaps `e`'s
    /// parent operand to an `Operand::Value`, and declines an aggregate value —
    /// the pool models scalars only, so a constant struct stays in skeleton form
    /// and only the scalars projected out of it are promoted. The scratch CTFE
    /// backend is a no-op (`false`) — its reads recompute the value through the
    /// value lattice, so it needs no node write-back.
    fn replace_with_value(&mut self, e: ExprId, value: Value) -> bool;
    /// Make `dst` take `src`'s content (`dst` becomes `src`).
    fn become_expr(&mut self, dst: ExprId, src: ExprId);
    fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId;
    fn alloc_stmt(&mut self, kind: StmtKind, span: crate::token::Span) -> StmtId;
    fn alloc_block(&mut self, stmts: Vec<StmtId>, span: crate::token::Span) -> BlockId;
    fn set_block_stmts(&mut self, block: BlockId, stmts: Vec<StmtId>);
}

/// In-place [`EditSink`] over a raw `Body`. Used for CTFE scratch reduction,
/// where the body is discarded after the value is read, so duplicated child
/// references and stale parent links do not matter.
pub(crate) struct BodySink<'a> {
    pub body: &'a mut Body,
}

impl EditSink for BodySink<'_> {
    fn body(&self) -> &Body {
        self.body
    }
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind) {
        self.body.exprs[e].kind = kind;
    }
    fn replace_with_value(&mut self, _e: ExprId, _value: Value) -> bool {
        // Scratch CTFE has no parent map to promote a constant into and pure
        // scalars no longer have a literal-node form; the value is recomputed on
        // read (`reduce_to_lattice_a` → `try_fold_a` over operands + env), so the
        // write-back is a no-op here.
        false
    }
    fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        // Clone src's whole node into dst (the original short-circuit rewrite
        // did `body.exprs[e] = body.exprs[keep].clone()`); the scratch body is
        // discarded, so the shared child references and the still-live `src`
        // node are harmless.
        let node = self.body.exprs[src].clone();
        self.body.exprs[dst] = node;
    }
    fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId {
        self.body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        })
    }
    fn alloc_stmt(&mut self, kind: StmtKind, span: crate::token::Span) -> StmtId {
        self.body.stmts.push(StmtNode { kind, span })
    }
    fn alloc_block(&mut self, stmts: Vec<StmtId>, span: crate::token::Span) -> BlockId {
        self.body.blocks.push(BlockNode { stmts, span })
    }
    fn set_block_stmts(&mut self, block: BlockId, stmts: Vec<StmtId>) {
        self.body.blocks[block].stmts = stmts;
    }
}

/// Whether `func` can be evaluated at compile time: pure, and concrete —
/// `type_params` and `impl_type_params` empty, since CTFE runs after
/// monomorphization.
///
/// `inline_hint` is deliberately not consulted: `#[inline(never)]` asks the
/// optimizer to keep the body out of line, which says nothing about whether
/// the result is knowable at compile time.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    // A unit call has no value to substitute for it.
    func.return_type != crate::tir::TypeTable::UNIT
        && func.effects.is_empty()
        && func.body.is_some()
        && !func.is_cm_binding
        && !func.is_dispatch_wrapper
        && !func.is_cm_export
        && !func.is_async
        && func.task_return_type.is_none()
        && func.stores.is_empty()
        && func.type_params.is_empty()
        && func.impl_type_params.is_empty()
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over the arena `Body`.
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
    /// `ExprKind::Local` consult this map during folding.
    ///
    /// Locals not present in the map default to [`Lattice::Unevaluated`].
    ///
    /// [`bind_local`]: Self::bind_local
    /// [`invalidate_local`]: Self::invalidate_local
    /// [`enter_function`]: Self::enter_function
    env: IndexMap<u32, Lattice>,
    ref_global_aliases: IndexMap<u32, GlobalKey>,
    /// The current function's [`aggregate_safe_locals`] — the only locals that
    /// may bind an aggregate constant. Cleared by [`Self::enter_function`], so
    /// an unpopulated set simply refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals of the CTFE frame currently executing that another handle may
    /// write — see [`clobbered_locals`]. Empty outside a frame.
    ctfe_clobbered: LocalSet,
    /// CTFE scratch-body fold memo: `expr → folded constant`. The scratch
    /// [`BodySink`] cannot promote a fold to an `Operand::Value` (no parent map)
    /// and pure scalars have no literal-node form, so a fold is recorded here and
    /// read back by [`Self::expr_to_lattice_a`]. Scoped to one `try_call_fold_a`
    /// call (saved/cleared around the scratch reduction); empty during real-body
    /// folding, where rewrites promote through the engine instead.
    scratch_folds: IndexMap<ExprId, Value>,
    /// Pre-built map of CTFE-eligible callees. When `None`, `Call` nodes
    /// stay [`Lattice::Unevaluated`]. The visitor populates this once
    /// per pass via [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    callees: Option<&'a CalleeMap>,
    /// When `None`, an `array_get` / `array_len` call is just an opaque call.
    seq_builtins: Option<&'a SeqBuiltinMap>,
    /// Pre-built lattice values for module-scope globals. When `None`,
    /// every `GlobalVarGet` stays [`Lattice::Unevaluated`]. The visitor
    /// populates this once per pass via [`with_globals`].
    ///
    /// [`with_globals`]: Self::with_globals
    globals: Option<&'a GlobalEnv>,
    /// Pre-built constant field values of module-scope globals. When `None`,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`]. The
    /// visitor populates this once per pass via [`with_global_fields`].
    ///
    /// [`with_global_fields`]: Self::with_global_fields
    global_fields: Option<&'a GlobalFieldEnv>,
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

fn let_ref_global(body: &Body, stmt: &StmtKind) -> Option<(u32, GlobalKey)> {
    let StmtKind::Let {
        local_index,
        value,
        is_mut,
        ..
    } = stmt
    else {
        return None;
    };
    if *is_mut {
        return None;
    }
    let ve = value.as_expr()?;
    let ExprKind::Unary {
        op: NirUnaryOp::Ref,
        expr,
    } = &body.exprs[ve].kind
    else {
        return None;
    };
    let ge = expr.as_expr()?;
    let ExprKind::GlobalVarGet {
        module_source,
        name,
    } = &body.exprs[ge].kind
    else {
        return None;
    };
    Some((*local_index, (module_source.clone(), name.clone())))
}

/// The local an lvalue or borrow chain roots at: `x`, `x.f`, `x[i]`, `*x`, and
/// any nesting of those.
fn lvalue_root_local(body: &Body, op: Operand) -> Option<u32> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
            lvalue_root_local(body, *inner)
        }
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => lvalue_root_local(body, *inner),
        _ => None,
    }
}

/// Every expression id reachable from the body root, in arena order.
fn reachable_exprs(body: &Body) -> Vec<ExprId> {
    struct Collect(Vec<ExprId>);
    impl NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node {
                self.0.push(e);
            }
            self.walk_node(body, node);
        }
    }
    // A body with no block structure is a bare expression, nothing orphaned.
    if body.blocks.is_empty() {
        return body.exprs.iter().map(|(e, _)| e).collect();
    }
    let mut collect = Collect(Vec::new());
    collect.visit_node(body, NodeRef::Block(body.root));
    collect.0
}

/// Locals of `body` that may bind an aggregate constant: every mention only
/// reads the value — as a field read's receiver or a `match` / `switch`
/// scrutinee — and no store target, mutable borrow, method receiver, or
/// mutable argument roots at them.
///
/// Only the reachable body is scanned: a mention orphaned by an earlier rewrite
/// cannot run, so it must not disqualify a local.
fn aggregate_safe_locals(body: &Body) -> LocalSet {
    fn disqualify_root(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            set.insert(index);
        }
    }
    let mut value_reads: IndexSet<ExprId> = IndexSet::default();
    let mut local_mentions: Vec<(ExprId, u32)> = Vec::new();
    let mut disqualified = LocalSet::default();
    for e in reachable_exprs(body) {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Local { index, .. } => local_mentions.push((e, *index)),
            ExprKind::FieldAccess { expr, .. }
            | ExprKind::Match { expr, .. }
            | ExprKind::Switch {
                scrutinee: expr, ..
            } => {
                if let Some(read) = expr.as_expr() {
                    value_reads.insert(read);
                }
            }
            ExprKind::Assign { target, .. } => {
                disqualify_root(body, (*target).into(), &mut disqualified);
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => disqualify_root(body, *expr, &mut disqualified),
            ExprKind::MethodCall { receiver, args, .. } => {
                disqualify_root(body, *receiver, &mut disqualified);
                for arg in args.iter().filter(|a| a.is_mut) {
                    disqualify_root(body, arg.expr, &mut disqualified);
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args.iter().filter(|a| a.is_mut) {
                    disqualify_root(body, arg.expr, &mut disqualified);
                }
            }
            _ => {}
        }
    }
    for (e, index) in &local_mentions {
        if !value_reads.contains(e) {
            disqualified.insert(*index);
        }
    }
    let mut safe = LocalSet::default();
    for (_, index) in local_mentions {
        if !disqualified.contains(index) {
            safe.insert(index);
        }
    }
    safe
}

fn local_binds_to_global_ref(body: &Body, local: u32, key: &GlobalKey) -> bool {
    body.stmts
        .iter()
        .any(|(_, st)| let_ref_global(body, &st.kind) == Some((local, key.clone())))
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            env: IndexMap::default(),
            ref_global_aliases: IndexMap::default(),
            aggregate_locals: LocalSet::default(),
            ctfe_clobbered: LocalSet::default(),
            scratch_folds: IndexMap::default(),
            callees: None,
            seq_builtins: None,
            globals: None,
            global_fields: None,
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

    /// Install the sequence-builtin lookup. Without it, an element or length
    /// read stays opaque.
    pub fn with_seq_builtins(&mut self, seq_builtins: &'a SeqBuiltinMap) -> &mut Self {
        self.seq_builtins = Some(seq_builtins);
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

    /// Install the [`GlobalFieldEnv`]. Without this,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`].
    /// Mirrors [`with_globals`](Self::with_globals).
    pub fn with_global_fields(&mut self, global_fields: &'a GlobalFieldEnv) -> &mut Self {
        self.global_fields = Some(global_fields);
        self
    }

    /// Override the per-pass CTFE step budget (default
    /// [`DEFAULT_STEP_BUDGET`]). Called rarely — primarily by tests
    /// exercising the budget-exhaustion path.
    pub fn set_step_budget(&mut self, budget: u32) -> &mut Self {
        self.step_budget = budget;
        self
    }

    fn global_field(&self, key: &GlobalKey, field_name: &str) -> Lattice {
        self.global_fields
            .and_then(|m| m.get(key))
            .and_then(|m| m.get(field_name))
            .cloned()
            .map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// Record `body`'s [`aggregate_safe_locals`]. The driving visitor calls
    /// this once per function, next to [`Self::record_ref_global_aliases`].
    pub fn record_aggregate_locals(&mut self, body: &Body) {
        self.aggregate_locals = aggregate_safe_locals(body);
    }

    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.ref_global_aliases.clear();
        let mut seen: IndexSet<u32> = IndexSet::default();
        for (_, st) in &body.stmts {
            if let Some((local, key)) = let_ref_global(body, &st.kind) {
                if seen.insert(local) {
                    self.ref_global_aliases.insert(local, key);
                } else {
                    self.ref_global_aliases.swap_remove(&local);
                }
            }
        }
    }

    /// Reset the per-function environment. The driving visitor must call
    /// this before walking each function body; otherwise a previous
    /// function's bindings would leak into the next one (local indices
    /// are unique per function, not project-wide).
    ///
    /// Asserts the recursion guard is clear — a leaked entry would mean
    /// a previous walk panicked mid-call.
    ///
    /// The step budget resets here, so one function with a long compile-time
    /// loop cannot decide whether the functions walked after it fold.
    pub fn enter_function(&mut self) {
        self.step_budget = DEFAULT_STEP_BUDGET;
        self.env.clear();
        self.ref_global_aliases.clear();
        self.aggregate_locals = LocalSet::default();
        self.ctfe_clobbered = LocalSet::default();
        self.scratch_folds.clear();
        debug_assert!(
            self.call_stack.is_empty(),
            "niri call_stack leaked across function boundary",
        );
    }

    /// Record a lattice value for a `let`-bound local. The driving
    /// visitor calls this after walking a `Let` statement: pass
    /// [`Lattice::Const`] for an immutable binding whose RHS reduced,
    /// [`Lattice::NonConst`] for `let mut` or any RHS that could not be
    /// reduced.
    ///
    /// An aggregate constant is only recorded for a local
    /// [`Self::record_aggregate_locals`] proved unreachable through any other
    /// handle; otherwise it degrades to [`Lattice::NonConst`].
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
        let unbacked_aggregate = matches!(&lattice, Lattice::Const(v) if !v.is_scalar())
            && !self.aggregate_locals.contains(index);
        let lattice = if unbacked_aggregate {
            Lattice::NonConst
        } else {
            lattice
        };
        self.env.insert(index, lattice);
    }

    /// The locals a match arm's pattern binds, with the values they take under
    /// a constant `scrutinee`. Empty unless the pattern definitely matches — an
    /// undecided pattern binds nothing knowable. The walker installs these
    /// ([`Self::enter_arm`]) around the arm's guard and body so both reduce
    /// under the values the arm would see at runtime.
    #[must_use]
    pub fn arm_bindings(&self, body: &Body, scrutinee: Operand, pattern: PatId) -> PatBindings {
        let Lattice::Const(value) = self.operand_to_lattice_a(body, scrutinee) else {
            return PatBindings::new();
        };
        let mut binds = PatBindings::new();
        match self.pattern_matches_a(body, &value, pattern, &mut binds) {
            PatternMatch::Yes => binds,
            PatternMatch::No | PatternMatch::Unknown => PatBindings::new(),
        }
    }

    /// Install `binds` for the walk of one match arm. Pass the returned scope
    /// to [`Self::leave_arm`] to put the environment back.
    pub fn enter_arm(&mut self, binds: &PatBindings) -> ArmScope {
        let scope = ArmScope(
            binds
                .iter()
                .map(|(index, _)| (*index, self.env.get(index).cloned()))
                .collect(),
        );
        for (index, value) in binds {
            self.bind_local(*index, Lattice::Const(value.clone()));
        }
        scope
    }

    /// The guard's value with `binds` in scope. The walker already reduced the
    /// guard there, so this is a read; scoping it again keeps the read from
    /// resolving a binding's index against whatever holds it outside the arm.
    fn guard_under_bindings(
        &mut self,
        body: &Body,
        guard: Operand,
        binds: &PatBindings,
    ) -> Option<bool> {
        let scope = self.enter_arm(binds);
        let value = self.operand_to_lattice_a(body, guard).as_const();
        self.leave_arm(scope);
        value.and_then(|v| v.as_bool())
    }

    pub fn leave_arm(&mut self, scope: ArmScope) {
        for (index, previous) in scope.0 {
            match previous {
                Some(lattice) => self.env.insert(index, lattice),
                None => self.env.swap_remove(&index),
            };
        }
    }

    /// Mark a local as definitely non-constant from this point on. The
    /// driving visitor calls this when it sees an `x = expr` assignment.
    /// Conservative — we don't track flow-sensitive new values, just
    /// invalidate the prior binding.
    pub fn invalidate_local(&mut self, index: u32) {
        self.env.insert(index, Lattice::NonConst);
    }

    /// Reads the bound env for locals and takes the SCCP join over
    /// `if` / `match` arms.
    /// Lattice value of an operand: the promoted constant for `Operand::Value`,
    /// else the skeleton subtree's lattice. Promoted pure values live in
    /// `body.values`, so a literal that left the skeleton still folds.
    pub fn operand_to_lattice_a(&self, body: &Body, op: Operand) -> Lattice {
        match op {
            Operand::Expr(e) => self.expr_to_lattice_a(body, e),
            Operand::Value(v) => self.value_to_lattice(body, v),
        }
    }

    /// Convert a promoted pure value to a `Lattice::Const` when it is a constant
    /// kind of a known primitive type; `Unevaluated` otherwise (a derived
    /// `Binary` / `Opaque` / non-primitive value niri does not evaluate here).
    fn value_to_lattice(&self, body: &Body, v: ValueId) -> Lattice {
        let Some(ty) = body.values.type_of(v) else {
            return Lattice::Unevaluated;
        };
        match body.values.kind(v) {
            ValueKind::Bool(b) => Lattice::Const(Value::Bool(*b)),
            ValueKind::Char(c) => Lattice::Const(Value::Char(*c)),
            ValueKind::Int(value, _) => {
                let Some(prim) = prim_of(ty, self.type_table).filter(|p| is_int_prim(*p)) else {
                    return Lattice::Unevaluated;
                };
                Lattice::Const(Value::Int {
                    value: *value,
                    prim,
                })
            }
            ValueKind::Float(bits, _) => {
                let prim = if is_f32_type(ty, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Lattice::Const(Value::Float {
                    value: f64::from_bits(*bits),
                    prim,
                })
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The global a field read resolves against: a direct `GLOBAL.f`, or a
    /// local bound to `&GLOBAL` earlier in this body.
    fn global_receiver_key(&self, body: &Body, inner: Operand) -> Option<GlobalKey> {
        match inner.as_expr().map(|e| &body.exprs[e].kind)? {
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => Some((module_source.clone(), name.clone())),
            ExprKind::Local { index, .. } => {
                let key = self.ref_global_aliases.get(index)?;
                debug_assert!(
                    local_binds_to_global_ref(body, *index, key),
                    "ref_global_aliases[{index}] = {key:?} is stale: the body being folded does \
                     not bind local {index} to that reference — per-function alias state leaked \
                     across a body boundary (e.g. a CTFE scratch reduction that did not \
                     save/clear it)",
                );
                Some(key.clone())
            }
            _ => None,
        }
    }

    /// The lattice of `receiver.field`: the [`GlobalFieldEnv`] entry for a
    /// global receiver, else the field projected out of a constant aggregate
    /// receiver (a literal, an env-bound local, or a CTFE-folded call result).
    ///
    /// The field env wins where it has an answer — it knows fields no
    /// initializer shows, such as the length body globalization records for a
    /// hoisted sequence — and otherwise the receiver's own value decides.
    fn field_access_lattice(
        &self,
        body: &Body,
        inner: Operand,
        field_index: u32,
        field_name: &str,
    ) -> Lattice {
        if let Some(key) = self.global_receiver_key(body, inner) {
            let known = self.global_field(&key, field_name);
            if !matches!(known, Lattice::Unevaluated) {
                return known;
            }
        }
        match self.operand_to_lattice_a(body, inner) {
            Lattice::Const(receiver) => receiver
                .field(field_index)
                .cloned()
                .map_or(Lattice::Unevaluated, Lattice::Const),
            Lattice::NonConst => Lattice::NonConst,
            Lattice::Unevaluated => Lattice::Unevaluated,
        }
    }

    /// Read an element out of a constant sequence. An index past the end is
    /// `NonConst`, so the run-time trap survives.
    fn index_lattice(&self, body: &Body, receiver: Operand, index: Operand) -> Lattice {
        let (Lattice::Const(receiver), Lattice::Const(index)) = (
            self.operand_to_lattice_a(body, receiver),
            self.operand_to_lattice_a(body, index),
        ) else {
            return Lattice::Unevaluated;
        };
        let Some((index, _)) = index.as_int() else {
            return Lattice::Unevaluated;
        };
        receiver
            .element(index)
            .cloned()
            .map_or(Lattice::NonConst, Lattice::Const)
    }

    /// `Const` only when every element is itself constant, and only up to
    /// [`MAX_SEQ_ELEMENTS`].
    fn seq_lattice(&self, body: &Body, type_id: TypeId, elements: &[Operand]) -> Lattice {
        let mut values = Vec::with_capacity(elements.len());
        for op in elements {
            match self.operand_to_lattice_a(body, *op) {
                Lattice::Const(v) => values.push(v),
                Lattice::NonConst => return Lattice::NonConst,
                Lattice::Unevaluated => return Lattice::Unevaluated,
            }
        }
        Value::seq(type_id, values).map_or(Lattice::NonConst, Lattice::Const)
    }

    /// The lattice of a struct / tuple literal: `Const` only when every field
    /// is itself constant, since a partially-known aggregate is not a value the
    /// engine can substitute or compare.
    fn aggregate_lattice(
        &self,
        body: &Body,
        type_id: TypeId,
        fields: impl Iterator<Item = (u32, Operand)>,
    ) -> Lattice {
        let mut values = Vec::new();
        let mut has_non_const = false;
        for (field_index, op) in fields {
            match self.operand_to_lattice_a(body, op) {
                Lattice::Const(v) => values.push((field_index, v)),
                Lattice::NonConst => has_non_const = true,
                Lattice::Unevaluated => return Lattice::Unevaluated,
            }
        }
        if has_non_const {
            return Lattice::NonConst;
        }
        Lattice::Const(Value::aggregate(type_id, values))
    }

    pub fn expr_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        // A scratch-CTFE fold memoized for `e` (no node form for pure scalars).
        if let Some(v) = self.scratch_folds.get(&e) {
            return Lattice::Const(v.clone());
        }
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Local { index, .. } => {
                self.env.get(index).cloned().unwrap_or(Lattice::Unevaluated)
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => self.field_access_lattice(body, *inner, *field_index, field_name),
            ExprKind::StructLiteral { fields, .. } => self.aggregate_lattice(
                body,
                node.type_id,
                fields.iter().map(|f| (f.field_index, f.value)),
            ),
            ExprKind::TupleLiteral { elements } => self.aggregate_lattice(
                body,
                node.type_id,
                elements
                    .iter()
                    .enumerate()
                    .map(|(i, op)| (u32::try_from(i).expect("tuple arity fits u32"), *op)),
            ),
            // An array literal denotes the whole container: `wir_build` lowers
            // it to `{ repr: array.new_fixed, used: N }`.
            ExprKind::ArrayLiteral { elements } => {
                match self.seq_lattice(body, node.type_id, elements) {
                    Lattice::Const(backing) => Lattice::Const(Value::aggregate(
                        node.type_id,
                        vec![
                            (SeqField::Backing.index(), backing),
                            (
                                SeqField::Len.index(),
                                Value::Int {
                                    value: elements.len() as u64,
                                    prim: PrimitiveType::I32,
                                },
                            ),
                        ],
                    )),
                    other => other,
                }
            }
            ExprKind::PackedArray(bytes) => {
                let elements = bytes
                    .iter()
                    .map(|b| Value::Int {
                        value: u64::from(*b),
                        prim: PrimitiveType::U8,
                    })
                    .collect();
                Value::seq(node.type_id, elements).map_or(Lattice::NonConst, Lattice::Const)
            }
            ExprKind::Index { expr: inner, index } => self.index_lattice(body, *inner, *index),
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            } => self.operand_to_lattice_a(body, *inner),
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name),
            ExprKind::Block(b) => self.block_lattice_a(body, *b),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.operand_to_lattice_a(body, *condition);
                match cond {
                    Lattice::Const(Value::Bool(true)) => self.block_lattice_a(body, *then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice_a(body, *eb),
                        None => Lattice::Unevaluated,
                    },
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice_a(body, *then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => {
                                arm_lattice_for_feasible_join(self.block_lattice_a(body, *eb))
                            }
                            None => Lattice::NonConst,
                        };
                        then_lat.join(else_lat)
                    }
                }
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => match scrutinee.as_expr() {
                Some(e) => self.match_lattice_a(body, e, arms),
                // A promoted-constant scrutinee is not evaluated here; the
                // flow-fold visitor collapses constant matches structurally.
                None => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// Fold a `Binary` / `Unary` / `Cast` of constant operands to a value;
    /// `NonConst` (not `Unevaluated`) when the op would trap, so the node survives.
    pub fn try_fold_a(&self, body: &Body, e: ExprId) -> Lattice {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Binary { left, op, right } => {
                let l = match self.operand_to_lattice_a(body, *left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                let r = match self.operand_to_lattice_a(body, *right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            // A shared borrow denotes what it points at rather than operating
            // on it. `eval_unary` has no rule for that and would bury the
            // referent's own constant as non-constant.
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                ..
            } => Lattice::Unevaluated,
            ExprKind::Unary { op, expr: inner } => {
                let v = match self.operand_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            ExprKind::Cast { expr: inner, .. } => {
                let Some(target) = prim_of(node.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                match self.operand_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => option_to_lattice(eval_cast(v, target)),
                    other => other,
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a block: its single tail `Expr`, else `Unevaluated`.
    fn block_lattice_a(&self, body: &Body, b: BlockId) -> Lattice {
        match body.blocks[b].stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &body.stmts[*single].kind {
                StmtKind::Expr(e) => self.operand_to_lattice_a(body, *e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a `match`: the chosen arm under a constant scrutinee,
    /// else the join over the feasible arms.
    fn match_lattice_a(&self, body: &Body, scrutinee: ExprId, arms: &[ArmData]) -> Lattice {
        let scrut_const = self.expr_to_lattice_a(body, scrutinee).as_const();
        if arms.is_empty() {
            return Lattice::Unevaluated;
        }
        if let Some(scrut_v) = scrut_const {
            let mut candidates = Vec::<Lattice>::new();
            let mut yes_found = false;
            for arm in arms {
                // Guards are decided by the rewrite path, which can scope the
                // pattern's bindings; here they leave the arm undecided.
                let pm = if arm.guard.is_some() {
                    PatternMatch::Unknown
                } else {
                    self.pattern_matches_a(body, &scrut_v, arm.pattern, &mut PatBindings::new())
                };
                let body_lat =
                    arm_lattice_for_feasible_join(self.operand_to_lattice_a(body, arm.body));
                match pm {
                    PatternMatch::No => {}
                    PatternMatch::Yes => {
                        if candidates.is_empty() {
                            return self.operand_to_lattice_a(body, arm.body);
                        }
                        candidates.push(body_lat);
                        yes_found = true;
                        break;
                    }
                    PatternMatch::Unknown => candidates.push(body_lat),
                }
            }
            if !yes_found {
                return Lattice::NonConst;
            }
            join_all(&candidates)
        } else {
            if !is_provably_exhaustive_a(body, arms) {
                return Lattice::NonConst;
            }
            let mut acc = Lattice::Unevaluated;
            for arm in arms {
                acc = acc.join(arm_lattice_for_feasible_join(
                    self.operand_to_lattice_a(body, arm.body),
                ));
            }
            acc
        }
    }

    /// Whether `value` matches `pat`, recording into `binds` the locals the
    /// pattern binds and the sub-values they take. `binds` is only meaningful
    /// on [`PatternMatch::Yes`]; a rejected alternative may have left entries
    /// behind.
    fn pattern_matches_a(
        &self,
        body: &Body,
        value: &Value,
        pat: PatId,
        binds: &mut PatBindings,
    ) -> PatternMatch {
        match &body.pats[pat].kind {
            PatKind::Wildcard => PatternMatch::Yes,
            PatKind::Binding { local_index, .. } => {
                binds.push((*local_index, value.clone()));
                PatternMatch::Yes
            }
            PatKind::Literal(lit) => match (lit, value) {
                (NirLiteralPattern::I128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_i128(*v, *prim, *p))
                }
                (NirLiteralPattern::U128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_u128(*v, *prim, *p))
                }
                (NirLiteralPattern::Bool(p), Value::Bool(v)) => bool_to_match(p == v),
                (NirLiteralPattern::Char(p), Value::Char(v)) => bool_to_match(p == v),
                (
                    NirLiteralPattern::I128(_)
                    | NirLiteralPattern::U128(_)
                    | NirLiteralPattern::Bool(_)
                    | NirLiteralPattern::Char(_),
                    _,
                ) => PatternMatch::No,
                (NirLiteralPattern::String(_) | NirLiteralPattern::Null, _) => {
                    PatternMatch::Unknown
                }
            },
            PatKind::Or(alts) => {
                let mut any_unknown = false;
                for alt in alts {
                    let mut alt_binds = PatBindings::new();
                    match self.pattern_matches_a(body, value, *alt, &mut alt_binds) {
                        PatternMatch::Yes => {
                            // Alternatives are tried in order at run time, so an
                            // undecided earlier one may be the one that matches
                            // — and it would bind from its own positions.
                            if any_unknown && !alt_binds.is_empty() {
                                return PatternMatch::Unknown;
                            }
                            binds.append(&mut alt_binds);
                            return PatternMatch::Yes;
                        }
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
            PatKind::Range {
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
            PatKind::ConstantValue { expr } => {
                match self.operand_to_lattice_a(body, *expr).as_const() {
                    Some(v) if &v == value => PatternMatch::Yes,
                    Some(_) => PatternMatch::No,
                    None => PatternMatch::Unknown,
                }
            }
            PatKind::Struct { fields, .. } => self.all_fields_match(
                body,
                value,
                fields.iter().map(|f| (f.field_index, f.pattern)),
                binds,
            ),
            // A tuple rest (`(a, ..)`) leaves the trailing sub-patterns without
            // a fixed element index, so only the exact-arity form is modelled.
            PatKind::Tuple(pats, has_rest) if !*has_rest => self.all_fields_match(
                body,
                value,
                pats.iter()
                    .enumerate()
                    .map(|(i, p)| (u32::try_from(i).expect("tuple arity fits u32"), *p)),
                binds,
            ),
            PatKind::Tuple(_, _) | PatKind::Variant { .. } | PatKind::Enum { .. } => {
                PatternMatch::Unknown
            }
        }
    }

    /// Conjunction of the sub-pattern results over an aggregate's fields:
    /// definitely-no as soon as one field rules the pattern out, definitely-yes
    /// only when every listed field matches. A field the value does not carry —
    /// or a sub-pattern the engine does not model — makes the whole pattern
    /// `Unknown`. A value that is not an aggregate is `Unknown` rather than
    /// vacuously matching a field-less pattern.
    fn all_fields_match(
        &self,
        body: &Body,
        value: &Value,
        fields: impl Iterator<Item = (u32, PatId)>,
        binds: &mut PatBindings,
    ) -> PatternMatch {
        if !matches!(value, Value::Aggregate { .. }) {
            return PatternMatch::Unknown;
        }
        let mut any_unknown = false;
        for (field_index, pat) in fields {
            let Some(field_value) = value.field(field_index) else {
                return PatternMatch::Unknown;
            };
            match self.pattern_matches_a(body, field_value, pat, binds) {
                PatternMatch::No => return PatternMatch::No,
                PatternMatch::Unknown => any_unknown = true,
                PatternMatch::Yes => {}
            }
        }
        if any_unknown {
            PatternMatch::Unknown
        } else {
            PatternMatch::Yes
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    // Arena rewriter. The arena counterparts of `reduce_local` /
    // `reduce_local_block` / `rewrite_if_expr` / `rewrite_match_expr` /
    // `try_call_fold`, mutating the `Body` the const-fold visitor walks.
    // ───────────────────────────────────────────────────────────────────────

    /// The single-node rewrites at `e` (no recursion into children).
    pub(crate) fn reduce_local_block_via<S: EditSink>(
        &mut self,
        sink: &mut S,
        block: BlockId,
    ) -> bool {
        let body = sink.body();
        let has_constant_if = body.blocks[block].stmts.iter().any(|s| {
            matches!(
                &body.stmts[*s].kind,
                StmtKind::If { condition, .. }
                    if operand_bool(body, *condition).is_some()
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = body.blocks[block].stmts.clone();
        let mut new_stmts: Vec<StmtId> = Vec::new();
        for s in old_stmts {
            let body = sink.body();
            let spliced = if let StmtKind::If {
                condition,
                then_block,
                else_block,
            } = &body.stmts[s].kind
            {
                operand_bool(body, *condition).map(|value| (value, *then_block, *else_block))
            } else {
                None
            };
            if let Some((value, then_block, else_block)) = spliced {
                if value {
                    new_stmts.extend(sink.body().blocks[then_block].stmts.clone());
                } else if let Some(eb) = else_block {
                    new_stmts.extend(sink.body().blocks[eb].stmts.clone());
                }
                continue;
            }
            new_stmts.push(s);
        }
        sink.set_block_stmts(block, new_stmts);
        true
    }

    /// In-place wrapper over [`Self::reduce_local_via`] for the CTFE
    /// scratch-body path.
    pub fn reduce_local_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let mut sink = BodySink { body };
        self.reduce_local_via(&mut sink, e)
    }

    /// Reduce `e` to its flow-sensitive constant value or collapse a constant
    /// branch, committing through `sink`. The value substitutions
    /// ([`Self::flow_fold_kind_a`]) and the structural collapses
    /// (short-circuit / `if` / `match`) all route through the sink, so the
    /// engine-routed visitor keeps the parent map / use index coherent and the
    /// scratch-body CTFE path mutates in place.
    pub(crate) fn reduce_local_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        if let Some(value) = self.flow_fold_value_a(sink.body(), e) {
            // Promote the folded scalar to an `Operand::Value` in `e`'s parent.
            if sink.replace_with_value(e, value.clone()) {
                return true;
            }
            // The scratch backend cannot promote (no parent map); memoize the
            // fold so the scratch's later lattice reads see the constant. Falling
            // through to the structural rewrites is a no-op for a pure constant.
            self.scratch_folds.insert(e, value);
        }
        if rewrite_short_circuit_via(sink, e) {
            return true;
        }
        if self.rewrite_if_expr_via(sink, e) {
            return true;
        }
        self.rewrite_match_expr_via(sink, e)
    }

    /// The environment-free constant value of `e`, as the literal [`ExprKind`]
    /// that should replace it, or `None` when `e` does not fold without
    /// per-function state.
    ///
    /// This is the subset of [`reduce_local_a`](Self::reduce_local_a) that
    /// depends only on the node and its (already-folded) children plus the
    /// program-wide [`CalleeMap`]: literal `Binary` / `Unary` / `Cast`
    /// arithmetic, projection out of a constant aggregate, and pure
    /// compile-time function evaluation. Only scalars are returned — an
    /// aggregate has no operand form. Local-bound constants and
    /// immutable-global reads stay with [`crate::optimize`]'s flow-sensitive
    /// const-fold walker, which owns the per-function dataflow state — an
    /// interpreter driving this must keep its `env` empty, since a projection's
    /// receiver resolves through it.
    ///
    /// Because the interpreter's `env` is empty here, `try_fold_a` and
    /// `try_call_fold_a` only succeed when every operand / argument is already
    /// a literal; the children a fold discards are therefore literal-only,
    /// never `Local` mentions. That lets the rewrite engine apply the result
    /// through its coherent edit API without the use index going stale.
    ///
    /// Unlike `reduce_local_a`, this does **not** mutate `body`: the engine rule
    /// promotes the returned value to an `Operand::Value` via
    /// `Engine::replace_expr_with_value`.
    pub fn const_fold_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate_a(body, e)
            .filter(Value::is_scalar)
    }

    fn const_fold_candidate_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold_a(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value_a(body, e) {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold_a(body, e) {
            return Some(v);
        }
        None
    }

    /// The constant a `receiver.field` node reads, when the receiver is a
    /// constant aggregate. Discarding the receiver is safe precisely because it
    /// is constant: a literal aggregate's fields are constants, and a call only
    /// reduces to one when it is CTFE-eligible (pure), so nothing observable is
    /// dropped and the read cannot trap on null.
    ///
    /// A call receiver is folded here rather than in
    /// [`Self::field_access_lattice`], which cannot run CTFE from `&self`; that
    /// is what lets `factory().field` reduce to the field of the constructed
    /// value.
    fn field_projection_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } = &body.exprs[e].kind
        else {
            return None;
        };
        let (inner, field_index) = (*inner, *field_index);
        if let Some(v) = self
            .field_access_lattice(body, inner, field_index, field_name)
            .as_const()
        {
            return Some(v);
        }
        let receiver = self.try_call_fold_a(body, inner.as_expr()?).as_const()?;
        receiver.field(field_index).cloned()
    }

    /// The flow-sensitive constant value of `e` — `env`-bound locals, immutable
    /// globals, literal arithmetic, aggregate field projection, and pure CTFE —
    /// or `None`. The structural rewrites (short-circuit / `if` / `match`
    /// collapse) are *not* included. The sink promotes the result to an
    /// `Operand::Value` via [`EditSink::replace_with_value`], so the value is
    /// always a scalar: a constant aggregate keeps its skeleton node and only
    /// the scalars projected out of it fold.
    pub fn flow_fold_value_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.flow_fold_candidate_a(body, e).filter(Value::is_scalar)
    }

    fn flow_fold_candidate_a(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold_a(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value_a(body, e) {
            return Some(v);
        }
        // A bare `Local` read bound to a constant in the flow env (the "env-bound
        // locals" this doc promises). `try_fold_a` only folds arith; consult the
        // env here so a `let x = <const>; … x …` that store→load forwarding missed
        // — a post-`inline` binding the build-once graph never valued — still
        // folds. Mutable locals are recorded `NonConst` (sound by flow), so this
        // is immutable-only and cannot stale.
        if matches!(&body.exprs[e].kind, ExprKind::Local { .. })
            && let Lattice::Const(v) = self.expr_to_lattice_a(body, e)
        {
            return Some(v);
        }
        if let ExprKind::GlobalVarGet {
            module_source,
            name,
        } = &body.exprs[e].kind
            && let Lattice::Const(v) = self.global_lattice(module_source, name)
        {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold_a(body, e) {
            return Some(v);
        }
        None
    }

    /// Splice a constant-condition `if` statement into its parent block.
    /// In-place wrapper over [`Self::reduce_local_block_via`] for the CTFE
    /// scratch-body path; the engine-routed visitor uses the `via` form with
    /// an `EngineSink`.
    pub fn reduce_local_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let mut sink = BodySink { body };
        self.reduce_local_block_via(&mut sink, block)
    }

    /// Bottom-up reduce the subtree rooted at `e` over the kinds the engine
    /// understands (Binary / Unary / Cast / If / Match), applying
    /// [`Self::reduce_local_a`] at each node so a child fold is observable at
    /// its parent. Used by CTFE (`try_call_fold_a`) to evaluate a callee tail
    /// whose children no outer walk has pre-reduced.
    /// Reduce an operand in place: a no-op (`false`) for a promoted pure value
    /// (already reduced), else reduce the skeleton subtree.
    fn reduce_in_place_operand_a(&mut self, body: &mut Body, op: Operand) -> bool {
        op.as_expr()
            .is_some_and(|e| self.reduce_in_place_a(body, e))
    }

    pub fn reduce_in_place_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let mut changed = match &body.exprs[e].kind {
            ExprKind::Binary { left, right, .. } => {
                let (l, r) = (*left, *right);
                let a = self.reduce_in_place_operand_a(body, l);
                let b = self.reduce_in_place_operand_a(body, r);
                a || b
            }
            ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
                let i = *inner;
                self.reduce_in_place_operand_a(body, i)
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let (c, t, e2) = (*condition, *then_branch, *else_branch);
                let mut ch = self.reduce_in_place_operand_a(body, c);
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arm_data: Vec<(Option<Operand>, PatId, Operand)> =
                    arms.iter().map(|a| (a.guard, a.pattern, a.body)).collect();
                let mut ch = self.reduce_in_place_operand_a(body, scrutinee);
                for (guard, pattern, arm_body) in arm_data {
                    let binds = self.arm_bindings(body, scrutinee, pattern);
                    let scope = self.enter_arm(&binds);
                    if let Some(g) = guard {
                        ch |= self.reduce_in_place_operand_a(body, g);
                    }
                    ch |= self.reduce_in_place_operand_a(body, arm_body);
                    self.leave_arm(scope);
                }
                ch
            }
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                self.reduce_in_place_operand_a(body, inner)
            }
            ExprKind::StructLiteral { fields, .. } => {
                let values: Vec<Operand> = fields.iter().map(|f| f.value).collect();
                let mut ch = false;
                for v in values {
                    ch |= self.reduce_in_place_operand_a(body, v);
                }
                ch
            }
            ExprKind::TupleLiteral { elements } => {
                let elements = elements.clone();
                let mut ch = false;
                for v in elements {
                    ch |= self.reduce_in_place_operand_a(body, v);
                }
                ch
            }
            _ => false,
        };
        changed |= self.reduce_local_a(body, e);
        changed
    }

    /// Block-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let stmts = body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in stmts {
            changed |= self.reduce_in_place_stmt_a(body, s);
        }
        changed |= self.reduce_local_block_a(body, block);
        changed
    }

    /// Statement-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_stmt_a(&mut self, body: &mut Body, s: StmtId) -> bool {
        match &body.stmts[s].kind {
            StmtKind::Expr(e) => {
                let e = *e;
                self.reduce_in_place_operand_a(body, e)
            }
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                let v = *value;
                self.reduce_in_place_operand_a(body, v)
            }
            StmtKind::Return { value } | StmtKind::Break { value, .. } => match *value {
                Some(v) => self.reduce_in_place_operand_a(body, v),
                None => false,
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (c, t, e2) = (*condition, *then_block, *else_block);
                let mut ch = c
                    .as_expr()
                    .is_some_and(|ce| self.reduce_in_place_a(body, ce));
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            StmtKind::Loop { body: b } => {
                let b = *b;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::LabeledBlock { block, .. } => {
                let b = *block;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::Continue => false,
        }
    }

    /// Project `e` to a lattice, assuming its children are already reduced (the
    /// const-fold visitor walks bottom-up): `try_fold_a` sees folded children
    /// directly, and a non-foldable node falls through to `expr_to_lattice_a`.
    pub fn reduce_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        match self.try_fold_a(body, e) {
            Lattice::Unevaluated => self.expr_to_lattice_a(body, e),
            other => other,
        }
    }

    /// Reduce the subtree bottom-up in place (so multi-level constant operands
    /// fold), then project to a lattice. The standalone entry point for callers
    /// with an unreduced expression — the `niri` unit tests.
    pub fn reduce_to_lattice_full_a(&mut self, body: &mut Body, e: ExprId) -> Lattice {
        self.reduce_in_place_a(body, e);
        self.reduce_to_lattice_a(body, e)
    }

    /// Collapse an `if` with a constant condition or equal arms.
    fn rewrite_if_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let (condition, then_branch, else_branch) = match &sink.body().exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => (*condition, *then_branch, *else_branch),
            _ => return false,
        };
        let cond_lat = self.operand_to_lattice_a(sink.body(), condition);

        // (1) Constant condition → splice the chosen arm.
        if let Lattice::Const(Value::Bool(b)) = cond_lat {
            let span = sink.body().exprs[e].span;
            let kind = if b {
                ExprKind::Block(then_branch)
            } else if let Some(eb) = else_branch {
                ExprKind::Block(eb)
            } else {
                // `if false {}` with no else evaluates to unit; an empty block
                // is the unit-typed skeleton form (the unit value has no node).
                ExprKind::Block(sink.alloc_block(Vec::new(), span))
            };
            sink.replace_kind(e, kind);
            return true;
        }

        // (2)/(3) require both arms Const.
        let Lattice::Const(t) = self.block_lattice_a(sink.body(), then_branch) else {
            return false;
        };
        let Some(eb) = else_branch else {
            return false;
        };
        let Lattice::Const(ev) = self.block_lattice_a(sink.body(), eb) else {
            return false;
        };

        // (2) Bool-arms collapse.
        if let (Value::Bool(t_b), Value::Bool(e_b)) = (&t, &ev)
            && t_b != e_b
        {
            if *t_b {
                // `if c { true } else { false }` ≡ `c`. Splice the skeleton
                // condition in place; a promoted value has no node to clone.
                let Some(cond_e) = condition.as_expr() else {
                    return false;
                };
                let cond_kind = sink.body().exprs[cond_e].kind.clone();
                sink.replace_kind(e, cond_kind);
            } else {
                sink.replace_kind(
                    e,
                    ExprKind::Unary {
                        op: NirUnaryOp::Not,
                        expr: condition,
                    },
                );
            }
            return true;
        }

        // (3) Both-arms-equal collapse.
        if t != ev {
            return false;
        }
        if !condition
            .as_expr()
            .is_none_or(|ce| is_speculatable_a(sink.body(), ce))
        {
            return false;
        }
        // Promote both-equal arms to the shared constant. The scratch backend
        // declines (no parent map); its read path recomputes, so report no change.
        sink.replace_with_value(e, t)
    }

    /// Collapse a `match` with a constant scrutinee or a bool-discriminator shape.
    fn rewrite_match_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let body = sink.body();
        let scrutinee = match &body.exprs[e].kind {
            ExprKind::Match { expr, arms } if !arms.is_empty() => *expr,
            _ => return false,
        };
        let arms_data: Vec<(Option<Operand>, PatId, Operand, crate::token::Span)> =
            match &body.exprs[e].kind {
                ExprKind::Match { arms, .. } => arms
                    .iter()
                    .map(|a| (a.guard, a.pattern, a.body, a.span))
                    .collect(),
                _ => unreachable!(),
            };

        // Rule 1: const scrutinee → splice the chosen arm.
        if let Lattice::Const(scrut_v) = self.operand_to_lattice_a(sink.body(), scrutinee) {
            let mut chosen: Option<(usize, PatBindings)> = None;
            for (i, (guard, pat, _, _)) in arms_data.iter().enumerate() {
                let mut binds = PatBindings::new();
                match self.pattern_matches_a(sink.body(), &scrut_v, *pat, &mut binds) {
                    PatternMatch::No => continue,
                    PatternMatch::Unknown => return false,
                    PatternMatch::Yes => {}
                }
                // A guard reads the arm's bindings, so it is only meaningful
                // with them in scope. An undecided one may still be taken,
                // leaving every later arm unreachable.
                match guard {
                    None => {}
                    Some(g) => match self.guard_under_bindings(sink.body(), *g, &binds) {
                        Some(true) => {}
                        Some(false) => continue,
                        None => return false,
                    },
                }
                chosen = Some((i, binds));
                break;
            }
            let Some((idx, binds)) = chosen else {
                return false;
            };
            let (body_op, arm_span) = (arms_data[idx].2, arms_data[idx].3);
            // Splicing the arm strips its pattern, so a binding the body still
            // reads would be left dangling.
            if operand_reads_any_local(sink.body(), body_op, &binds) {
                return false;
            }
            // The chosen arm's value becomes `e`'s value, wrapped in a block. A
            // promoted constant arm flows straight into the `Operand` statement
            // slot — no node materialization (WEP: The Live ValueGraph).
            let span = match body_op {
                Operand::Expr(ex) => sink.body().exprs[ex].span,
                Operand::Value(_) => arm_span,
            };
            let stmt = sink.alloc_stmt(StmtKind::Expr(body_op), span);
            let block = sink.alloc_block(vec![stmt], span);
            sink.replace_kind(e, ExprKind::Block(block));
            return true;
        }

        // Rule 2: `match X { Pat => true, _ => false } → <discriminator>`.
        // The scrutinee is preserved inside the synthesised `Binary`, and the
        // `Match` node `e` keeps its own span — only its `kind` is replaced.
        if let Some(replacement) = try_match_bool_discriminator_a(sink.body(), &arms_data) {
            let right = sink.alloc_expr(
                ExprKind::EnumConstruct {
                    enum_type: replacement.enum_type,
                    case_index: replacement.case_index,
                    case_name: replacement.case_name,
                },
                replacement.enum_type,
                replacement.span,
            );
            sink.replace_kind(
                e,
                ExprKind::Binary {
                    left: scrutinee,
                    op: NirBinaryOp::Eq,
                    right: right.into(),
                },
            );
            return true;
        }

        // Rule 3: non-const speculatable scrutinee, all-arms-equal. A promoted
        // `Operand::Value` scrutinee is a constant — trivially speculatable.
        if let Some(e) = scrutinee.as_expr()
            && !is_speculatable_a(sink.body(), e)
        {
            return false;
        }
        if arms_data.iter().any(|(g, _, _, _)| g.is_some()) {
            return false;
        }
        let arms_for_exh: Vec<ArmData> = match &sink.body().exprs[e].kind {
            ExprKind::Match { arms, .. } => arms.clone(),
            _ => unreachable!(),
        };
        if !is_provably_exhaustive_a(sink.body(), &arms_for_exh) {
            return false;
        }
        let mut common: Option<Value> = None;
        for (_, _, b, _) in &arms_data {
            let Lattice::Const(v) = self.operand_to_lattice_a(sink.body(), *b) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(c) if c != v => return false,
                Some(_) => {}
            }
        }
        let v = common.expect("at least one arm");
        // Promote all-equal arms to the shared constant; the scratch backend
        // declines (recomputes on read), so report its no-change honestly.
        sink.replace_with_value(e, v)
    }

    fn exec_block_a(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let stmts = body.blocks[block].stmts.clone();
        let mut value = Lattice::Unevaluated;
        for s in stmts {
            match self.exec_stmt_a(body, s) {
                Flow::Fallthrough(v) => value = v,
                other => return other,
            }
        }
        Flow::Fallthrough(value)
    }

    /// Execute one statement, charging the step budget.
    ///
    /// A statement counts as executed only when everything it evaluates lands
    /// on a constant. Reducing an expression is not performing it, so anything
    /// left undone — an unfolded call, a global write, a would-be trap —
    /// abandons the evaluation rather than being stepped past.
    fn exec_stmt_a(&mut self, body: &mut Body, s: StmtId) -> Flow {
        if self.step_budget == 0 {
            return Flow::Bail;
        }
        self.step_budget -= 1;
        match &body.stmts[s].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let (index, value) = (*local_index, *value);
                match self.eval_operand_a(body, value) {
                    lattice @ Lattice::Const(_) => {
                        self.bind_ctfe_local(index, lattice);
                        Flow::Fallthrough(Lattice::Unevaluated)
                    }
                    Lattice::NonConst | Lattice::Unevaluated => Flow::Bail,
                }
            }
            StmtKind::Expr(op) => {
                let op = *op;
                self.exec_expr_stmt_a(body, op)
            }
            StmtKind::Return { value } => {
                let lattice = self.eval_optional_operand_a(body, *value);
                Flow::Return(lattice)
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
                match self.eval_operand_a(body, condition) {
                    Lattice::Const(Value::Bool(true)) => self.exec_block_a(body, then_block),
                    Lattice::Const(Value::Bool(false)) => match else_block {
                        Some(eb) => self.exec_block_a(body, eb),
                        None => Flow::Fallthrough(Lattice::Unevaluated),
                    },
                    Lattice::Const(
                        Value::Int { .. }
                        | Value::Float { .. }
                        | Value::Char(_)
                        | Value::Aggregate { .. }
                        | Value::Seq { .. },
                    )
                    | Lattice::NonConst
                    | Lattice::Unevaluated => Flow::Bail,
                }
            }
            StmtKind::Break { label, value } => {
                let (label, value) = (label.clone(), *value);
                let value = self.eval_optional_operand_a(body, value);
                Flow::Break { label, value }
            }
            StmtKind::Continue => Flow::Continue,
            StmtKind::Loop { body: block } => {
                let block = *block;
                self.exec_loop_a(body, block)
            }
            StmtKind::LabeledBlock { label, block } => {
                let (label, block) = (label.clone(), *block);
                match self.exec_block_a(body, block) {
                    Flow::Break {
                        label: Some(broke),
                        value,
                    } if broke == label => Flow::Fallthrough(value),
                    other => other,
                }
            }
            StmtKind::LetDestructure { .. } => Flow::Bail,
        }
    }

    /// Run a loop until it breaks, control leaves the function, or the budget
    /// runs out. Termination rests on the budget alone — the per-iteration
    /// charge covers an empty body too — so no constant trip count is needed.
    fn exec_loop_a(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let snapshot = LoopSnapshot::capture(body, block);
        loop {
            if self.step_budget == 0 {
                return Flow::Bail;
            }
            self.step_budget -= 1;
            match self.exec_block_a(body, block) {
                Flow::Fallthrough(_) | Flow::Continue => {}
                // A labeled `break` belongs to an enclosing labeled block.
                Flow::Break { label: None, .. } => {
                    return Flow::Fallthrough(Lattice::Unevaluated);
                }
                other => return other,
            }
            snapshot.restore(body);
            self.scratch_folds.clear();
        }
    }

    /// An assignment updates the environment; anything else contributes its
    /// value as the block's result.
    fn exec_expr_stmt_a(&mut self, body: &mut Body, op: Operand) -> Flow {
        if let Some(e) = op.as_expr()
            && let ExprKind::Assign { target, value } = &body.exprs[e].kind
        {
            let (target, value) = (*target, *value);
            let Some(index) = assign_target_local(body, target) else {
                return Flow::Bail;
            };
            let Lattice::Const(v) = self.eval_operand_a(body, value) else {
                return Flow::Bail;
            };
            self.bind_ctfe_local(index, Lattice::Const(v));
            return Flow::Fallthrough(Lattice::Unevaluated);
        }
        match self.eval_operand_a(body, op) {
            lattice @ Lattice::Const(_) => Flow::Fallthrough(lattice),
            Lattice::NonConst | Lattice::Unevaluated => Flow::Bail,
        }
    }

    fn eval_optional_operand_a(&mut self, body: &mut Body, op: Option<Operand>) -> Lattice {
        match op {
            Some(op) => self.eval_operand_a(body, op),
            None => Lattice::Unevaluated,
        }
    }

    /// Reducing in place first is what lets a nested call fold before the
    /// operand is projected.
    fn eval_operand_a(&mut self, body: &mut Body, op: Operand) -> Lattice {
        match op.as_expr() {
            Some(e) => {
                self.reduce_in_place_a(body, e);
                self.reduce_to_lattice_a(body, e)
            }
            None => self.operand_to_lattice_a(body, op),
        }
    }

    /// Bind a local inside a compile-time frame. A local the frame may reach
    /// through another handle keeps no value.
    fn bind_ctfe_local(&mut self, index: u32, lattice: Lattice) {
        if self.ctfe_clobbered.contains(index) {
            self.env.insert(index, Lattice::NonConst);
        } else {
            self.bind_local(index, lattice);
        }
    }

    /// Evaluate `array_get(seq, i)` / `array_len(seq)` over a constant
    /// sequence. The argument is a reference to the array, and a reference to a
    /// constant reads as that constant, so no separate deref step is needed.
    fn try_seq_builtin_fold_a(&self, body: &Body, e: ExprId) -> Lattice {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return Lattice::Unevaluated;
        };
        let Some(builtin) = self.seq_builtins.and_then(|m| m.get(func_id)) else {
            return Lattice::Unevaluated;
        };
        match (builtin, args.as_slice()) {
            (SeqBuiltin::Len, [arr]) => {
                let Lattice::Const(v) = self.operand_to_lattice_a(body, arr.expr) else {
                    return Lattice::Unevaluated;
                };
                v.seq_len().map_or(Lattice::Unevaluated, |len| {
                    Lattice::Const(Value::Int {
                        value: len as u64,
                        prim: PrimitiveType::I32,
                    })
                })
            }
            (SeqBuiltin::Get, [arr, index]) => self.index_lattice(body, arr.expr, index.expr),
            (SeqBuiltin::Len | SeqBuiltin::Get, _) => Lattice::Unevaluated,
        }
    }

    /// Fold a pure call whose args are all constant: bind the params, run the
    /// callee's body, and return `Const(v)` only when it produces a value.
    /// `Unevaluated` on any miss, so the original call — and any runtime trap
    /// inside it — survives.
    fn try_call_fold_a(&mut self, body: &Body, e: ExprId) -> Lattice {
        if let lattice @ (Lattice::Const(_) | Lattice::NonConst) =
            self.try_seq_builtin_fold_a(body, e)
        {
            // `NonConst` here is an out-of-range read: keep the call so the
            // trap survives.
            return match lattice {
                Lattice::Const(v) => Lattice::Const(v),
                Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
            };
        }
        let Some(callees) = self.callees else {
            return Lattice::Unevaluated;
        };
        let (key, args): (CalleeKey, Vec<Operand>) = match &body.exprs[e].kind {
            ExprKind::Call { func_id, args, .. } => {
                (*func_id, args.iter().map(|a| a.expr).collect())
            }
            // Only a free `Call` is a CTFE-eligible in-package callee.
            _ => return Lattice::Unevaluated,
        };
        let Some(callee_rc) = callees.get(&key) else {
            return Lattice::Unevaluated;
        };
        if self.call_stack.iter().any(|k| k == &key) {
            return Lattice::Unevaluated;
        }
        let Ok(callee) = callee_rc.try_borrow() else {
            return Lattice::Unevaluated;
        };
        let mut bound: Vec<Value> = Vec::with_capacity(args.len());
        for arg in &args {
            match self.operand_to_lattice_a(body, *arg).as_const() {
                Some(v) => bound.push(v),
                None => return Lattice::Unevaluated,
            }
        }
        if bound.len() != callee.params.len() {
            return Lattice::Unevaluated;
        }
        let Some(callee_body) = callee.body.as_ref() else {
            return Lattice::Unevaluated;
        };
        if self.step_budget == 0 {
            return Lattice::Unevaluated;
        }
        self.step_budget -= 1;
        self.call_stack.push(key);
        let saved_env = std::mem::take(&mut self.env);
        // The scratch fold memo is scoped to this reduction; nested CTFE calls
        // get a fresh map and ids never cross scratch bodies.
        let saved_folds = std::mem::take(&mut self.scratch_folds);
        let saved_aliases = std::mem::take(&mut self.ref_global_aliases);
        // Local indices are per-function, so the caller's read-only-local set
        // says nothing about the callee's.
        let saved_aggregates = std::mem::take(&mut self.aggregate_locals);
        let saved_clobbered = std::mem::take(&mut self.ctfe_clobbered);
        // Execute on a scratch copy so the shared callee body, held under an
        // immutable `Ref`, is not mutated.
        let mut scratch = callee_body.nodes_only_clone();
        self.record_aggregate_locals(&scratch);
        self.ctfe_clobbered = clobbered_locals(&scratch);
        for (i, v) in bound.into_iter().enumerate() {
            let index = u32::try_from(i).expect("param count fits u32");
            let lattice = if self.ctfe_clobbered.contains(index) {
                Lattice::NonConst
            } else {
                Lattice::Const(v)
            };
            self.env.insert(index, lattice);
        }
        let root = scratch.root;
        let result = match self.exec_block_a(&mut scratch, root) {
            Flow::Return(lattice) | Flow::Fallthrough(lattice) => lattice,
            Flow::Break { .. } | Flow::Continue | Flow::Bail => Lattice::Unevaluated,
        };
        self.env = saved_env;
        self.scratch_folds = saved_folds;
        self.ref_global_aliases = saved_aliases;
        self.aggregate_locals = saved_aggregates;
        self.ctfe_clobbered = saved_clobbered;
        self.call_stack.pop();
        match result {
            c @ Lattice::Const(_) => c,
            Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
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
            .cloned()
            .unwrap_or(Lattice::Unevaluated)
    }
}

/// Whether the subtree under `op` reads any of the locals `binds` binds.
fn operand_reads_any_local(body: &Body, op: Operand, binds: &PatBindings) -> bool {
    struct Reads<'a> {
        binds: &'a PatBindings,
        found: bool,
    }
    impl NirRefVisitor for Reads<'_> {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node
                && let ExprKind::Local { index, .. } = &body.exprs[e].kind
                && self.binds.iter().any(|(bound, _)| bound == index)
            {
                self.found = true;
            }
            self.walk_node(body, node);
        }
    }
    let Some(expr) = op.as_expr() else {
        return false;
    };
    let mut visitor = Reads {
        binds,
        found: false,
    };
    visitor.visit_node(body, NodeRef::Expr(expr));
    visitor.found
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

/// Outcome of testing a pattern against a constant scrutinee
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

/// How control left a statement sequence during compile-time execution.
///
/// `Break` is not loop-specific: a labeled block is the value-producing shape
/// a `break L: v` completes, and a loop catches only an unlabeled `break`.
enum Flow {
    /// Reached the end, carrying the value of the last statement that
    /// produced one.
    Fallthrough(Lattice),
    Return(Lattice),
    Break {
        label: Option<String>,
        value: Lattice,
    },
    Continue,
    /// The engine could not follow the program: abandon the evaluation so the
    /// original call survives.
    Bail,
}

/// The loop-body nodes one iteration may rewrite, kept so the next iteration
/// starts from the same program. Reduction rewrites an expression toward the
/// value it took *this* time round — `acc + 1` becomes the literal `1` on the
/// first pass — so without restoring, later iterations read the first one's
/// results.
struct LoopSnapshot {
    exprs: Vec<(ExprId, ExprNode)>,
    stmts: Vec<(StmtId, StmtNode)>,
    blocks: Vec<(BlockId, Vec<StmtId>)>,
}

impl LoopSnapshot {
    fn capture(body: &Body, block: BlockId) -> Self {
        #[derive(Default)]
        struct Collect {
            exprs: Vec<ExprId>,
            stmts: Vec<StmtId>,
            blocks: Vec<BlockId>,
        }
        impl NirRefVisitor for Collect {
            fn visit_node(&mut self, body: &Body, node: NodeRef) {
                match node {
                    NodeRef::Expr(e) => self.exprs.push(e),
                    NodeRef::Stmt(s) => self.stmts.push(s),
                    NodeRef::Block(b) => self.blocks.push(b),
                    // Patterns are matched, never rewritten.
                    NodeRef::Pat(_) => {}
                }
                self.walk_node(body, node);
            }
        }
        let mut collect = Collect::default();
        collect.visit_node(body, NodeRef::Block(block));
        Self {
            exprs: collect
                .exprs
                .into_iter()
                .map(|e| (e, body.exprs[e].clone()))
                .collect(),
            stmts: collect
                .stmts
                .into_iter()
                .map(|s| (s, body.stmts[s].clone()))
                .collect(),
            blocks: collect
                .blocks
                .into_iter()
                .map(|b| (b, body.blocks[b].stmts.clone()))
                .collect(),
        }
    }

    /// Put the captured nodes back. Nodes an iteration allocated are left
    /// behind unreferenced.
    fn restore(&self, body: &mut Body) {
        for (e, node) in &self.exprs {
            body.exprs[*e] = node.clone();
        }
        for (s, node) in &self.stmts {
            body.stmts[*s] = node.clone();
        }
        for (b, stmts) in &self.blocks {
            body.blocks[*b].stmts.clone_from(stmts);
        }
    }
}

/// Locals of `body` that a compile-time frame cannot track, because something
/// other than a bare assignment at statement position can write them: a
/// borrow, a mutable argument, a method receiver, a store through a
/// projection, or an assignment buried inside a larger expression.
fn clobbered_locals(body: &Body) -> LocalSet {
    fn disqualify(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            set.insert(index);
        }
    }
    // Assignments the executor applies itself. Any other assignment is only
    // reduced, never run, so its target would keep a stale value.
    let mut executed: IndexSet<ExprId> = IndexSet::default();
    for (_, stmt) in &body.stmts {
        if let StmtKind::Expr(op) = &stmt.kind
            && let Some(e) = op.as_expr()
            && matches!(body.exprs[e].kind, ExprKind::Assign { .. })
        {
            executed.insert(e);
        }
    }
    let mut set = LocalSet::default();
    for (e, node) in &body.exprs {
        match &node.kind {
            ExprKind::Assign { target, .. } => {
                if !executed.contains(&e)
                    || !matches!(body.exprs[*target].kind, ExprKind::Local { .. })
                {
                    disqualify(body, (*target).into(), &mut set);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => disqualify(body, *expr, &mut set),
            ExprKind::MethodCall { receiver, args, .. } => {
                disqualify(body, *receiver, &mut set);
                for arg in args.iter().filter(|a| a.is_mut) {
                    disqualify(body, arg.expr, &mut set);
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args.iter().filter(|a| a.is_mut) {
                    disqualify(body, arg.expr, &mut set);
                }
            }
            _ => {}
        }
    }
    set
}

fn assign_target_local(body: &Body, target: ExprId) -> Option<u32> {
    match &body.exprs[target].kind {
        ExprKind::Local { index, .. } => Some(*index),
        _ => None,
    }
}

/// The locals a matched pattern binds, paired with the scrutinee sub-values
/// they take.
pub type PatBindings = Vec<(u32, Value)>;

/// The environment entries [`Interpreter::enter_arm`] displaced, restored by
/// [`Interpreter::leave_arm`].
pub struct ArmScope(Vec<(u32, Option<Lattice>)>);

fn bool_to_match(b: bool) -> PatternMatch {
    if b {
        PatternMatch::Yes
    } else {
        PatternMatch::No
    }
}

/// Join a slice of lattice values via [`Lattice::join`]. Empty input
/// returns [`Lattice::Unevaluated`] (the join's identity).
fn join_all(lats: &[Lattice]) -> Lattice {
    let mut acc = Lattice::Unevaluated;
    for l in lats {
        acc = acc.join(l.clone());
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

/// Whether the arms cover every scrutinee (a guardless catch-all exists).
fn is_provably_exhaustive_a(body: &Body, arms: &[ArmData]) -> bool {
    arms.iter()
        .any(|a| a.guard.is_none() && pattern_is_catch_all_a(body, a.pattern))
}

fn pattern_is_catch_all_a(body: &Body, pat: PatId) -> bool {
    match &body.pats[pat].kind {
        PatKind::Wildcard | PatKind::Binding { .. } => true,
        PatKind::Or(alts) => alts.iter().any(|p| pattern_is_catch_all_a(body, *p)),
        _ => false,
    }
}

/// Simplify `false && x` / `true || x` and their mirror forms.
fn rewrite_short_circuit_via<S: EditSink>(sink: &mut S, e: ExprId) -> bool {
    let body = sink.body();
    let keep: Operand = match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let (left, op, right) = (*left, *op, *right);
            match (operand_bool(body, left), op, operand_bool(body, right)) {
                (Some(false), NirBinaryOp::Or, _) | (Some(true), NirBinaryOp::And, _) => right,
                (_, NirBinaryOp::Or, Some(false)) | (_, NirBinaryOp::And, Some(true)) => left,
                _ => return false,
            }
        }
        _ => return false,
    };
    // Become the kept operand. The other operand is left orphaned. A constant
    // `keep` (a fully-constant short-circuit) is left to the const-fold path.
    let Some(keep_e) = keep.as_expr() else {
        return false;
    };
    sink.become_expr(e, keep_e);
    true
}

/// The boolean value of an operand: a promoted `ValueKind::Bool` in the pool.
/// `None` for any other operand.
fn operand_bool(body: &Body, op: Operand) -> Option<bool> {
    match body.values.kind(op.as_value()?) {
        ValueKind::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Recognize `match X { Case => true, _ => false }` as an equality test.
fn try_match_bool_discriminator_a(
    body: &Body,
    arms: &[(Option<Operand>, PatId, Operand, crate::token::Span)],
) -> Option<EnumEqReplacement> {
    let [yes_arm, no_arm] = arms else {
        return None;
    };
    if yes_arm.0.is_some() || no_arm.0.is_some() {
        return None;
    }
    if !matches!(body.pats[no_arm.1].kind, PatKind::Wildcard) {
        return None;
    }
    if operand_bool(body, yes_arm.2) != Some(true) {
        return None;
    }
    if operand_bool(body, no_arm.2) != Some(false) {
        return None;
    }
    let PatKind::Enum {
        enum_type,
        case_name,
        case_index,
    } = &body.pats[yes_arm.1].kind
    else {
        return None;
    };
    Some(EnumEqReplacement {
        enum_type: *enum_type,
        case_index: *case_index,
        case_name: case_name.clone(),
        span: yes_arm.3,
    })
}

/// Whether `e` can be evaluated out of order (side-effect-free, cannot trap).
fn is_speculatable_a(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_speculatable_operand_a(body, *left)
                && is_speculatable_operand_a(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_speculatable_operand_a(body, *inner)
        }
        ExprKind::Cast { expr: inner, .. } => is_speculatable_operand_a(body, *inner),
        ExprKind::FieldAccess { expr: inner, .. } => is_speculatable_operand_a(body, *inner),
        _ => false,
    }
}

/// Operand form of [`is_speculatable_a`]: a promoted pure value (constant)
/// is always speculatable.
fn is_speculatable_operand_a(body: &Body, op: crate::nir_arena::Operand) -> bool {
    op.as_expr().is_none_or(|e| is_speculatable_a(body, e))
}

// ──────────────────────────────────────────────────────────────────────────────
// `match X { Pat => true, _ => false }` discriminator collapse
// ──────────────────────────────────────────────────────────────────────────────

/// The replacement shape produced by `try_match_bool_discriminator`. The
/// scrutinee box is plugged in by the caller once it has taken ownership
/// of the original `Match` expression.
///
/// Only the `EnumEq` shape exists today; `PatKind::Variant` is left
/// intact because synthesising the matching `VariantTest` requires a
/// variant→case-index lookup that the pattern itself doesn't carry
/// (the WIR builder resolves it via the variant decl's case list,
/// which the interpreter doesn't carry today). The fpfmt motivator
/// (`SpecialKind`) is an `enum`, so the Enum-only scope is sufficient
/// for this PR; expanding to `Variant` is a follow-up.
struct EnumEqReplacement {
    enum_type: TypeId,
    case_index: u32,
    case_name: String,
    span: crate::token::Span,
}

impl EnumEqReplacement {}
