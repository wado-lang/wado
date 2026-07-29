//! NIR Interpreter (niri).
//!
//! Compile-time partial evaluator over the arena `Body`: it reduces what it can
//! and leaves a residual otherwise. Constant folding is the primary consumer;
//! branch pruning, constant propagation and compile-time function evaluation
//! reuse the same engine.
//!
//! Reduction is monotone — an expression only moves toward literal form, never
//! back — and idempotent. A literal leaf is left as written, so the source repr
//! (`0xFF`) survives a no-op pass.
//!
//! Each module below answers one question:
//!
//! - `lattice` — what an expression denotes.
//! - `frame` — what running a body does.
//! - `rewrite` — what becomes of an expression once its value is known.
//! - `trackability` — which locals a walk may hold a value for.
//! - `pattern` — whether a pattern matches a value.
//! - `place` — what a borrow or lvalue chain names.
//!
//! What the engine can evaluate is the WEP's to state, and it is maintained
//! there rather than here: `docs/wep-2026-04-27-nir-interpreter.md`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::const_eval::Value;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{
    BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, LocalSet, NodeRef, Operand, PatId,
    StmtId, StmtKind, StmtNode,
};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{TypeId, TypeTable};

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
/// Membership answers whether a frame may *run* the callee at all —
/// [`is_ctfe_runnable`], decided once at construction and never re-checked.
/// Whether the call's value may be substituted for it is a different
/// question, and it is answered per call: a unit callee denotes nothing, and
/// one writing through a `&mut` parameter runs only at statement position,
/// where the executor applies the write-backs. Arity, argument reduction and
/// body shape are likewise checked at fold time.
pub type CalleeMap = IndexMap<CalleeKey, Callee>;

/// A callee the engine may run, with the parameter facts the trackability
/// analysis needs answered without a borrow. Asking the function later answers
/// only when nobody holds `borrow_mut` on it, and a fold must not turn on
/// which function the visitor happens to be walking.
pub struct Callee {
    pub func: Rc<RefCell<NirFunction>>,
    pub mut_params: Vec<bool>,
    pub stored_params: Vec<bool>,
}

impl Callee {
    #[must_use]
    pub fn new(func: Rc<RefCell<NirFunction>>) -> Self {
        let (mut_params, stored_params) = {
            let borrowed = func.borrow();
            (
                borrowed.params.iter().map(|p| p.is_mut_ref).collect(),
                borrowed
                    .params
                    .iter()
                    .map(|p| borrowed.stores.contains(&p.name))
                    .collect(),
            )
        };
        Self {
            func,
            mut_params,
            stored_params,
        }
    }

    fn writes_receiver(&self) -> bool {
        self.writes_param(0)
    }

    fn arity(&self) -> usize {
        self.mut_params.len()
    }

    /// A `&mut T` borrow is the only parameter kind that reaches the caller's
    /// storage. An index the signature does not have answers as one that does:
    /// nothing about a call the map cannot account for is exempt.
    fn writes_param(&self, index: usize) -> bool {
        self.mut_params.get(index).copied().unwrap_or(true)
    }

    /// A stored parameter outlives the call, so naming its referent is not the
    /// passing read the other arguments are.
    fn reads_only(&self, index: usize) -> bool {
        self.stored_params.get(index).is_some_and(|stored| !stored)
    }
}

/// The builtins the engine evaluates. An element read or write reaches NIR as
/// a call to one of these, not as an `Index` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtfeBuiltin {
    ArrayGet,
    ArrayLen,
    ArrayNew,
    ArraySet,
    ArrayCopy,
    ColdPath,
    Select,
}

impl CtfeBuiltin {
    /// Whether the builtin writes its first argument. A write is performed at
    /// statement position rather than folded, and only by a frame that owns
    /// the place it lands in.
    fn is_write(self) -> bool {
        match self {
            Self::ArraySet | Self::ArrayCopy => true,
            Self::ArrayGet | Self::ArrayLen | Self::ArrayNew | Self::ColdPath | Self::Select => {
                false
            }
        }
    }
}

/// Which sequence builtin each callee id is, for the ids that are one.
pub type CtfeBuiltinMap = IndexMap<CalleeKey, CtfeBuiltin>;

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

mod frame;
mod lattice;
mod pattern;
mod place;
mod rewrite;
mod trackability;

use pattern::PatternMatch;
use trackability::{Reached, aggregate_safe_locals};

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
    /// Intern a constant into the function's value pool and return it as an
    /// operand. A scalar has no literal-node form, so a synthesized one in an
    /// operand position goes through the pool.
    fn const_operand(&mut self, kind: ValueKind, type_id: TypeId) -> Operand;
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
    fn const_operand(&mut self, kind: ValueKind, type_id: TypeId) -> Operand {
        Operand::Value(self.body.values.alloc_unshared(kind, type_id))
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

/// Whether a compile-time frame can run `func`'s body: pure, and concrete —
/// `type_params` and `impl_type_params` empty, since CTFE runs after
/// monomorphization.
///
/// `inline_hint` is deliberately not consulted: `#[inline(never)]` asks the
/// optimizer to keep the body out of line, which says nothing about whether
/// the result is knowable at compile time.
///
/// Nor is `stores`. The engine has no reference values — an argument reduces to
/// its referent's value — so what the callee keeps is a snapshot, sound exactly
/// while nothing can write the referent afterwards. [`Reached`] is what
/// holds that: a stored argument is the one read it does not exempt.
#[must_use]
pub fn is_ctfe_runnable(func: &NirFunction) -> bool {
    func.effects.is_empty()
        && func.body.is_some()
        && !func.is_cm_binding
        && !func.is_dispatch_wrapper
        && !func.is_cm_export
        && !func.is_async
        && func.task_return_type.is_none()
        && func.type_params.is_empty()
        && func.impl_type_params.is_empty()
}

/// Whether `func`'s call can be replaced by the value it computes: runnable,
/// producing a value at all, and keeping no reference past the call.
///
/// Strictly stronger than [`is_ctfe_runnable`], which is what the
/// [`CalleeMap`] gates on: a frame runs a unit callee for the writes it
/// performs, so requiring a value here would refuse work the frame does. This
/// is the question a caller asks when it wants to hold the result — hoisting
/// a pure call's result to a global, where nothing remains to write through.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    // A unit call has no value to substitute for it.
    func.return_type != crate::tir::TypeTable::UNIT
        && func.stores.is_empty()
        && is_ctfe_runnable(func)
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Everything the engine knows keyed by local index, which is per-function, so
/// entering a body means replacing the whole group and leaving one means
/// putting it back. Grouped rather than swapped field by field: a compile-time
/// frame exchanges all of it at once, and a member restored out of step with
/// its siblings would let one body read another's locals.
#[derive(Default)]
struct FrameState {
    /// Lattice values for the `let`-bound locals of the body being walked.
    /// Populated by the driving visitor via [`Interpreter::bind_local`] /
    /// [`Interpreter::invalidate_local`]. Absent locals read as
    /// [`Lattice::Unevaluated`].
    env: IndexMap<u32, Lattice>,
    ref_global_aliases: IndexMap<u32, GlobalKey>,
    /// The body's [`aggregate_safe_locals`] — the only locals that may bind an
    /// aggregate constant. An unpopulated set refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals a compile-time frame cannot track — see [`clobbered_locals`].
    /// Empty outside a frame.
    ctfe_clobbered: LocalSet,
    /// CTFE scratch-body fold memo: `expr → folded constant`. The scratch
    /// [`BodySink`] cannot promote a fold to an `Operand::Value` (no parent map)
    /// and pure scalars have no literal-node form, so a fold is recorded here
    /// and read back by [`Interpreter::expr_to_lattice_a`]. Empty during
    /// real-body folding, where rewrites promote through the engine instead.
    scratch_folds: IndexMap<ExprId, Value>,
}

/// Partial evaluator over the arena `Body`.
///
/// Holds the type table needed to resolve operand widths, the [`FrameState`] of
/// the body being walked, an optional [`CalleeMap`] of runnable callees, a step
/// budget, and a `call_stack` of in-flight CTFE frames for recursion detection.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    frame: FrameState,
    /// Pre-built map of CTFE-eligible callees. When `None`, `Call` nodes
    /// stay [`Lattice::Unevaluated`]. The visitor populates this once
    /// per pass via [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    callees: Option<&'a CalleeMap>,
    /// When `None`, an `array_get` / `array_len` call is just an opaque call.
    ctfe_builtins: Option<&'a CtfeBuiltinMap>,
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

/// What running a call left behind: the value it returned, and what each `&mut`
/// parameter holds at return, paired with the caller place it belongs in.
struct CallRun {
    result: Lattice,
    writes: Vec<(u32, Vec<u32>, Value)>,
}

/// Every expression id reachable from the body root, in arena order.
pub(crate) fn reachable_exprs(body: &Body) -> Vec<ExprId> {
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
            frame: FrameState::default(),
            callees: None,
            ctfe_builtins: None,
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
    pub fn with_ctfe_builtins(&mut self, ctfe_builtins: &'a CtfeBuiltinMap) -> &mut Self {
        self.ctfe_builtins = Some(ctfe_builtins);
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

    /// Install `state`, handing back what it displaced. Every per-local fact
    /// moves together, so there is no window in which one body's locals are
    /// read against another's — which is what a compile-time frame needs, and
    /// what makes leaving one a single statement that cannot be skipped.
    fn swap_frame(&mut self, state: FrameState) -> FrameState {
        std::mem::replace(&mut self.frame, state)
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
        let writes = Reached::outside_frame(body, self.ctfe_builtins, self.callees);
        self.frame.aggregate_locals = aggregate_safe_locals(body, &writes);
    }

    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.frame.ref_global_aliases.clear();
        let mut seen: IndexSet<u32> = IndexSet::default();
        for (_, st) in &body.stmts {
            if let Some((local, key)) = let_ref_global(body, &st.kind) {
                if seen.insert(local) {
                    self.frame.ref_global_aliases.insert(local, key);
                } else {
                    self.frame.ref_global_aliases.swap_remove(&local);
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
        self.frame = FrameState::default();
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
            && !self.frame.aggregate_locals.contains(index);
        let lattice = if unbacked_aggregate {
            Lattice::NonConst
        } else {
            lattice
        };
        self.frame.env.insert(index, lattice);
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
                .map(|(index, _)| (*index, self.frame.env.get(index).cloned()))
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
                Some(lattice) => self.frame.env.insert(index, lattice),
                None => self.frame.env.swap_remove(&index),
            };
        }
    }

    /// Mark a local as definitely non-constant from this point on. The
    /// driving visitor calls this when it sees an `x = expr` assignment.
    /// Conservative — we don't track flow-sensitive new values, just
    /// invalidate the prior binding.
    pub fn invalidate_local(&mut self, index: u32) {
        self.frame.env.insert(index, Lattice::NonConst);
    }
}

/// The locals a matched pattern binds, paired with the scrutinee sub-values
/// they take.
pub type PatBindings = Vec<(u32, Value)>;

/// The environment entries [`Interpreter::enter_arm`] displaced, restored by
/// [`Interpreter::leave_arm`].
pub struct ArmScope(Vec<(u32, Option<Lattice>)>);

